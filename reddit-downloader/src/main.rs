use futures::StreamExt;
use itertools::Itertools;
use post_subscriber::SubscriberClient;
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::tokio_serde::formats::Bincode;
use tracing::{debug, info, warn, error};
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use std::collections::HashMap;
use std::iter::{self, FromIterator};
use tokio::time::{sleep, Duration};
use futures::lock::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use truncrate::*;

use anyhow::{anyhow, Context};

use redis::AsyncCommands;

use reqwest::header::{USER_AGENT, HeaderMap};
use std::env;
use futures_util::TryStreamExt;

use mongodb::bson::{doc, Document};
use mongodb::options::{ClientOptions, FindOptions};
use serde_json::Value::Null;

use tarpc::{client, context, serde_transport::Transport};

mod downloaders;


/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue<'a> {
    U64(u64),
    Bool(bool),
    String(String),
    DownloaderClient(downloaders::client::Client<'a>),
    SubscriberClient(post_subscriber::SubscriberClient),
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct<'a> {
    _value: HashMap<String, ConfigValue<'a>>
}

#[derive(Debug, Clone)]
struct SubredditState {
    /// Name of the subreddit.
    name: String,
    /// Unix timestamp of the last time the subreddit was fetched. None if has never been fetched.
    last_fetched: Option<u64>,
    /// The ID of the last post fetched for resumption purposes. None if no posts have been fetched yet.
    fetched_up_to: Option<String>,
    /// How many more pages of results to fetch.
    pages_left: u64,
}

/// Represents a Reddit post
#[derive(Debug, Clone)]
pub struct Post {
    /// The score (upvotes-downvotes) of the post
    score: i64,
    /// A URl to the post
    url: String,
    title: String,
    /// Embed URL of the post media
    embed_url: Option<String>,
    /// Name of the author of the post
    author: String,
    /// The post's unique ID
    id: String,
    /// The timestamp of the post's creation
    timestamp: u64,
}

impl From<Post> for Vec<(String, String)> {
    #[tracing::instrument]
    fn from(post: Post) -> Vec<(String, String)> {
        let mut tmp = vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url.to_string()),
            ("title".to_string(), post.title.to_string()),
            ("author".to_string(), post.author.to_string()),
            ("id".to_string(), post.id.to_string()),
            ("timestamp".to_string(), post.timestamp.to_string())
        ];

        if post.embed_url.is_some() {
            tmp.push(("embed_url".to_string(), post.embed_url.unwrap()));
        };

        tmp
    }
}


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> Result<u64, anyhow::Error> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis() as u64)
}

/// Request an access token from Reddit
///
/// Returns the token and the Unix timestamp at which it expires.
/// # Arguments
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## web_client
/// The [web_client](reqwest::Client) to make requests with.
/// ## device_id
/// If specified, will request and [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
#[tracing::instrument]
async fn request_access_token(reddit_client: String, reddit_secret: String, web_client: &reqwest::Client, device_id: Option<String>) -> Result<(String, u64), anyhow::Error> {
    let post_data = match device_id {
        Some(x) => format!("grant_type=https://oauth.reddit.com/grants/installed_client&\\device_id={}", x),
        None => "grant_type=client_credentials".to_string(),
    };

    let res = web_client
        .post("https://www.reddit.com/api/v1/access_token")
        .body(post_data)
        .basic_auth(reddit_client, Some(reddit_secret))
        .send()
        .await?;

    let text = match res.text().await {
        Ok(x) => x,
        Err(x) => {
            let txt = format!("Failed to get text from reddit: {}", x);
            warn!("{}", txt);
            sentry::capture_message(&txt, sentry::Level::Warning);
            return Err(x)?;
        }
    };
    debug!("Matched text");
    let results: serde_json::Value = match serde_json::from_str(&text) {
        Ok(x) => x,
        Err(x) => {
            let txt = format!("Failed to parse JSON from Reddit: {}", text);
            warn!("{}", txt);
            sentry::capture_message(&txt, sentry::Level::Warning);
            return Err(x)?;
        }
    };

    debug!("Reddit access token response: {:?}", results);
    let token = results.get("access_token").expect("Reddit did not return access token").to_string();
    let expires_in:u64 = results.get("expires_in").expect("Reddit did not provide expires_in").as_u64().unwrap();
    let expires_at = get_epoch_ms()? + expires_in*1000;

    debug!("New token expires in {} seconds, or at {}", expires_in, expires_at);

    return Ok((token, expires_at));
}

/// Returns a String of the Reddit access token to use
///
/// # Arguments
/// ## con
/// The [connection](redis::aio::MultiplexedConnection) to the redis DB.
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## device_id
/// If specified, will request an [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
#[tracing::instrument(skip(con, web_client))]
async fn get_access_token(con: &mut redis::aio::MultiplexedConnection, reddit_client: String, reddit_secret: String, web_client: &reqwest::Client, device_id: Option<String>) -> Result<String, anyhow::Error> { // Return a reddit access token
    let token_name = match device_id.clone() { // The key to grab from the DB
        Some(x) => x,
        _ => "default".to_string(),
    };


    let token = con.hget("reddit_tokens", token_name.clone()).await;
    let token = match token {
        Ok(x) => x,
        Err(_) => {
            debug!("Requesting new access token, none exists");
            let token_results = request_access_token(reddit_client.clone(), reddit_secret.clone(), web_client, device_id.clone()).await?;
            let access_token = token_results.0;
            let expires_at = token_results.1;

            con.hset("reddit_tokens", token_name.clone(), format!("{},{}", access_token, expires_at)).await?;

            format!("{},{}", access_token, expires_at)
        }
    };

    let expires_at:u64 = token.split(",").collect::<Vec<&str>>()[1].parse()?;
    let mut access_token:String = token.split(",").collect::<Vec<&str>>()[0].parse::<String>()?.replace('"', "");

    if expires_at < get_epoch_ms()? {
        debug!("Requesting new access token, current one expired");
        let token_results = request_access_token(reddit_client.clone(), reddit_secret.clone(), web_client, device_id.clone()).await?;
        access_token = token_results.0;
        let expires_at = token_results.1;

        con.hset("reddit_tokens", token_name, format!("{},{}", access_token, expires_at)).await?;
    }

    debug!("Reddit Token: {}", access_token.replace("\"", ""));
    return Ok(access_token);
}

/// Get the top 1000 most recent media posts and store them in the DB
/// Returns the ID of the last post processed, or None if there were no posts.
/// 
/// # Arguments
/// ## subreddit
/// A [String](String) representing the subreddit name, without the `r/`
/// ## con
/// A [Connection](redis::aio::MultiplexedConnection) to the Redis DB
/// ## web_client
/// The Reqwest [Client](reqwest::Client)
/// ## reddit_client
/// The Reddit client as [String](String)
/// ## reddit_secret
/// The Reddit access token as a [String](String)
/// ## device_id
/// None if a default subreddit, otherwise is the user's ID.
#[tracing::instrument(skip(con, web_client, downloaders_client))]
async fn get_subreddit(subreddit: String, con: &mut redis::aio::MultiplexedConnection, web_client: &reqwest::Client, reddit_client: String, reddit_secret: String, device_id: Option<String>, imgur_client: String, mut after: Option<String>, pages: Option<u8>, downloaders_client: &downloaders::client::Client<'_>, subscriber: SubscriberClient) -> Result<Option<String>, anyhow::Error> { // Get the top 1000 most recent posts and store them in the DB
    debug!("Fetching subreddit: {:?}", subreddit);

    let access_token = get_access_token(con, reddit_client.clone(), reddit_secret.clone(), web_client, device_id).await?;

    let url_base = format!("https://oauth.reddit.com/r/{}/hot.json?limit=100", subreddit);
    let mut url = String::new();

    let existing_posts: Vec<String> = redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(-1i64).query_async(con).await
        .context("Getting existing posts")?;
    debug!("Existing posts: {:?}", existing_posts);
    
    // Wrap data that needs to be shared between threads in Arcs
    let existing_posts = Arc::new(existing_posts);
    let subreddit = Arc::new(subreddit);

    // Fetch pages of posts
    for x in 0..pages.unwrap_or(10) {
        debug!("Making {}th request to reddit", x);

        // If we haven't constructed the URL yet
        if url.len() == 0 {
            url = url_base.clone();
            // If we aren't on the first page, add the after parameter to specify the last post we processed
            match &after {
                Some(x) => {
                    url = format!("{}&after={}", url, x.clone());
                },
                None => {},
            }
        }

        // Set headers to tell Reddit who we are
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, format!("Discord:RSlash:v{} (by /u/murrax2)", env!("CARGO_PKG_VERSION")).parse()?);
        headers.insert("Authorization", format!("bearer {}", access_token.clone()).parse()?);

        debug!("{}", url);
        debug!("{:?}", headers);

        let res = web_client
            .get(&url)
            .headers(headers)
            .send()
            .await?;

        debug!("Finished request");
        debug!("Response Headers: {:?}", res.headers());

        // Get current time, so we know when next to fetch this subreddit
        let last_requested = get_epoch_ms()?;

        // Process text response from Reddit
        let text = match res.text().await {
            Ok(x) => x,
            Err(x) => {
                let txt = format!("Failed to get text from reddit: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        };

        debug!("Response successfully processed as text");

        // Convert text response to JSON
        let results: serde_json::Value = match serde_json::from_str(&text) {
            Ok(x) => x,
            Err(_) => {
                let txt = format!("Failed to parse JSON from Reddit: {}", text);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        };

        // Extract the array of posts from the JSON
        let results = match results.get("data") {
            Some(x) => match x.get("children") {
                Some(x) => match x.as_array() {
                    Some(x) => x,
                    None => {
                        let txt = format!("Failed to convert field `children` in reddit response to array: {}", text);
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                        return Ok(after);
                    }
                },
                None => {
                    let txt = format!("Failed to get field `children` in reddit response: {}", text);
                    warn!("{}", txt);
                    sentry::capture_message(&txt, sentry::Level::Warning);
                    return Ok(after);
                }
            },
            None => {
                let txt = format!("Failed to get field `data` in reddit response: {}", text);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        };

        if results.len() == 0 {
            debug!("No results");
            break;
        }

        // Store last post so we can resume from it next iteration
        after = match results[results.len() -1 ]["data"]["id"].as_str() {
            Some(x) => Some(x.to_string()),
            None => {
                None
            }
        };

        let mut posts: Vec<Result<(usize, serde_json::Map<String, serde_json::Value>), anyhow::Error>> = Vec::new();

        let mut count = 0;
        for post in results {
            posts.push(Ok((count, post["data"].as_object().ok_or(anyhow!("Reddit response json not object"))?.to_owned())));
            count += 1;
        }

        let stream = futures::stream::iter(posts);

        let posts: Arc<Mutex<HashMap<usize, String>>> = Arc::new(Mutex::new(HashMap::new()));

        stream.for_each_concurrent(5, |result| {
            // Clone Arcs so they can be moved into the async block and each thread gets its own reference
            let posts = Arc::clone(&posts);
            let existing_posts = Arc::clone(&existing_posts);
            let mut con = con.clone();
            let subreddit = Arc::clone(&subreddit);
            let subscriber = subscriber.clone();

            async move {
                let (i, post) = match result {
                    Ok(x) => x,
                    Err(x) => {
                        let txt = format!("Failed to get post: {}", x);
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                        return;
                    }
                };
                debug!("{:?} - {:?}", post["title"], post["url"]);

                // If the post has been removed by a moderator, skip it
                if post.get("removed_by_category").unwrap_or(&Null) != &Null {
                    debug!("Post removed by moderator");
                    if existing_posts.contains(&format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))) {
                        debug!("Removing post from DB");
                        match con.lrem::<String, String, usize>(format!("subreddit:{}:posts", subreddit), 0, format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))).await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to remove post from DB: {}", x);
                                warn!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Warning);
                            }
                        };
                        match con.del::<String, usize>(format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))).await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to remove post from DB: {}", x);
                                warn!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Warning);
                            }
                        };
                    }
                    return;
                }
    
                // If the post has been removed by the author, skip it
                if post["author"].to_string().replace('"', "") == "[deleted]" {
                    debug!("Post removed by author");
                    if existing_posts.contains(&format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))) {
                        debug!("Removing post from DB");
                        match con.lrem::<String, String, usize>(format!("subreddit:{}:posts", subreddit), 0, format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))).await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to remove post from DB: {}", x);
                                warn!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Warning);
                            }
                        };
                        match con.del::<String, usize>(format!("subreddit:{}:post:{}", subreddit, &post["id"].to_string().replace('"', ""))).await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to remove post from DB: {}", x);
                                warn!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Warning);
                            }
                        };
                    }
                    return;
                }
    
                if post["title"].to_string() == "Flipping a pancake" { // For some reason pinned images aren't marked as pinned on the api, and this one post is pinned but doesn't embed.
                    return;
                }

                // Redis key for this post
                let key: String = format!("subreddit:{}:post:{}", post["subreddit"].to_string().replace("\"", ""), &post["id"].to_string().replace('"', ""));

                let exists = existing_posts.contains(&key);

                if exists {
                    debug!("Post already exists in DB");
                }

                // Fetch URL if this is a new post
                let url = if exists {
                    None
                } else {
                    let url = post["url"].to_string().replace('"', "");
                    let url = if url.contains("v.redd.it") {
                        post["media"]["reddit_video"]["dash_url"].to_string().replace('"', "")
                    } else {
                        url
                    };

                    // If URL is already embeddable, no further processing is needed
                    if (url.ends_with(".gif") || url.ends_with(".png") || url.ends_with(".jpg") || url.ends_with(".jpeg")) && !url.contains("redgifs.com") {
                        Some(url)
                    } else if url.ends_with(".mp4") || url.contains("imgur.com") || url.contains("redgifs.com") || url.contains(".mpd") {
                        debug!("URL is not embeddable, but we have the ability to turn it into one");
                        let mut res = match downloaders_client.request(&url, &post["id"].to_string().replace('"', "")).await {
                            Ok(x) => {
                                debug!("Downloaded media: {}", x);
                                x
                            },
                            Err(x) => {
                                let txt = format!("Failed to download media: {}", x);
                                warn!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Warning);
                                return;
                            }
                        };
                        if !res.starts_with("http") {
                            res = format!("https://r-slash.b-cdn.net/gifs/{}", res);
                        }
                        Some(res)
                    } else {
                        debug!("URL is not embeddable, and we do not have the ability to turn it into one.");
                        return;
                    }
                };

                let timestamp = post["created_utc"].as_f64().unwrap() as u64;
                let mut title = post["title"].to_string().replace('"', "").replace("&amp;", "&");
                // Truncate title length to 256 chars (Discord embed title limit)
                title = (*title.as_str()).truncate_to_boundary(256).to_string();


                let post_object = Post {
                    score: post["score"].as_i64().unwrap_or(0),
                    url: format!("https://reddit.com{}", post["permalink"].to_string().replace('"', "")),
                    title: title,
                    embed_url: url,
                    author: post["author"].to_string().replace('"', ""),
                    id: post["id"].to_string().replace('"', ""),
                    timestamp: timestamp,
                };

                debug!("Adding to redis {:?}", post_object);

                // Push post to Redis
                let value = Vec::from(post_object);

                match con.hset_multiple::<&str, String, String, redis::Value>(&key, &value).await {
                    Ok(_) => {},
                    Err(x) => {
                        let txt = format!("Failed to set post in redis: {}", x);
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                    }
                };

                match subscriber.notify(context::current(), subreddit.to_string(), post["id"].to_string().replace('"', "")).await {
                    Ok(_) => {},
                    Err(x) => {
                        let txt = format!("Failed to notify subscriber: {}", x);
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                    }
                };
                
                // Subreddit has not been downloaded before, meaning we should try get posts into Redis ASAP to minimise the time before the user gets a response
                if existing_posts.len() < 30 {
                    // Push post to list of posts
                    debug!("Pushing key: {}", key);
                    match con.rpush::<String, &str, redis::Value>(format!("subreddit:{}:posts", subreddit), &key).await {
                        Ok(_) => {},
                        Err(x) => {
                            let txt = format!("Failed to push post to list: {}", x);
                            warn!("{}", txt);
                            sentry::capture_message(&txt, sentry::Level::Warning);
                        }
                    };
                }
                
                // Push post to thread-safe hashmap
                let mut posts = posts.lock().await;
                posts.insert(i, key);
            }
        }
        ).await;

        // Create new list of all posts in 
        let mut keys = Vec::new();
        let posts = match Arc::try_unwrap(posts) {
            Ok(x) => x,
            Err(_) => {
                let txt = format!("Failed to unwrap Arc");
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        }.into_inner();
        for index in posts.keys().sorted() {
            keys.push(posts[index].clone());
        }


        // Push existing posts to the end of the list so they are not removed from the cache
        for key in &*existing_posts {
            if !keys.contains(&key) {
                keys.push(key.to_string());
            }
        }

        if keys.len() != 0 {
            redis::cmd("MULTI").query_async(con).await?;
            con.del(format!("subreddit:{}:posts", subreddit)).await?;
            con.rpush(format!("subreddit:{}:posts", subreddit), keys.clone()).await?;
            redis::cmd("EXEC").query_async(con).await?;
        };


        let time_since_request = get_epoch_ms()? - last_requested;
        if time_since_request < 2000 {
            debug!("Waiting {:?}ms", 2000 - time_since_request);
            sleep(Duration::from_millis(2000 - time_since_request)).await; // Reddit rate limit is 30 requests per minute, so must wait at least 2 seconds between requests
        }
    }   

    return Ok(after);
}


// Check if RediSearch index exists
#[tracing::instrument(skip(con))]
async fn index_exists(con: &mut redis::aio::MultiplexedConnection, index: String) -> bool {
    match redis::cmd("FT.INFO").arg(index).query_async::<redis::aio::MultiplexedConnection, redis::Value>(con).await {
        Ok(_) => true,
        Err(_) => false,
    }
}


// Create RediSearch index for subreddit
#[tracing::instrument(skip(con))]
async fn create_index(con: &mut redis::aio::MultiplexedConnection, index: String, prefix: String) -> Result<(), anyhow::Error>{
    Ok(redis::cmd("FT.CREATE")
        .arg(index)
        .arg("PREFIX")
        .arg("1")
        .arg(prefix)
        .arg("SCHEMA")
        .arg("title")
        .arg("TEXT")
        .arg("score")
        .arg("NUMERIC")
        .arg("SORTABLE")
        .arg("author")
        .arg("TEXT")
        .arg("SORTABLE")
        .arg("flair")
        .arg("TAG")
        .arg("timestamp")
        .arg("NUMERIC")
        .arg("SORTABLE")
        .query_async(con).await?)
}


/// Start the downloading loop
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn download_loop(data: Arc<Mutex<HashMap<String, ConfigValue<'_>>>>) -> Result<(), anyhow::Error>{
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_multiplexed_async_connection().await.expect("Can't connect to redis");

    let web_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Discord:RSlash:v1.0.1 (by /u/murrax2)")
        .build().expect("Failed to build client");

    let data_lock = data.lock().await;
    let reddit_secret = match data_lock.get("reddit_secret").unwrap() {
        ConfigValue::String(x) => x,
        _ => panic!("Failed to get reddit_secret")
    }.clone();

    let reddit_client = match data_lock.get("reddit_client").unwrap() {
        ConfigValue::String(x) => x,
        _ => panic!("Failed to get reddit_client")
    }.clone();

    let imgur_client = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");
    let do_custom = env::var("DO_CUSTOM").expect("DO_CUSTOM not set");

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some("Downloader".to_string());

    let mongodb_client = mongodb::Client::with_options(client_options).expect("failed to connect to mongodb");

    let imgur_client_id = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");

    let downloaders_client = downloaders::client::Client::new("/data/media", Some(imgur_client_id), None).await?;

    let reconnect_opts = ReconnectOptions::new()
        .with_exit_if_first_connect_fails(false)
        .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
    let tcp_stream = StubbornTcpStream::connect_with_options("post-subscriber.discord-bot-shared.svc.cluster.local:50051", reconnect_opts).await?;
    let transport = Transport::from((tcp_stream, Bincode::default()));

    let subscriber = post_subscriber::SubscriberClient::new(client::Config::default(), transport).spawn();

    debug!("Entering loop");
    if do_custom == "true".to_string() {
        debug!("Starting custom subreddit loop");
        let mut subreddits: HashMap<String, SubredditState> = HashMap::new();
        loop {
            // Populate subreddits with any new subreddit requests
            let custom_subreddits: Vec<String> = redis::cmd("LRANGE").arg("custom_subreddits_queue").arg(0i64).arg(-1i64).query_async(&mut con).await.unwrap_or(Vec::new());
            for subreddit in custom_subreddits {
                if !subreddits.contains_key(&subreddit) {
                    let last_fetched: Option<u64> = con.get(&subreddit).await?;
                    subreddits.insert(subreddit.clone(), SubredditState {
                        name: subreddit.clone(),
                        fetched_up_to: None,
                        last_fetched,
                        pages_left: 10,
                    });
                }
            }

            if subreddits.len() > 0 {
                debug!("Subreddits: {:?}", subreddits);
            }

            let mut next_subreddit = None;

            // If there's a subreddit that hasn't been fetched yet, make that the next one
            for subreddit in subreddits.values() {
                if subreddit.last_fetched.is_none() {
                    next_subreddit = Some(subreddit.name.clone());
                    debug!("Found subreddit that hasn't been fetched yet: {}", subreddit.name);
                    break;
                }
            }

            // If there's no subreddit that hasn't been fetched yet, find the one fetched least recently and make that the next one
            match next_subreddit {
                Some(_) => {},
                None => {
                    let mut min = None;
                    // Find the minimum last_fetched
                    for subreddit in subreddits.values() {
                        match min {
                            Some(x) => {
                                if subreddit.last_fetched.unwrap() < x {
                                    min = subreddit.last_fetched;
                                }
                            },
                            None => {
                                min = subreddit.last_fetched;
                            }
                        }
                    }

                    // Find subreddit with the correct last_fetched
                    for subreddit in subreddits.values() {
                        if subreddit.last_fetched == min {
                            next_subreddit = Some(subreddit.name.clone());
                            break;
                        }
                    }
                }
            }

            if next_subreddit.is_some() {
                debug!("Next subreddit: {:?}", next_subreddit);
            }

            if next_subreddit.is_some() {
                let subreddit = next_subreddit.unwrap();
                info!("Fetching subreddit {}", &subreddit);
                if !index_exists(&mut con, format!("idx:{}", &subreddit)).await {
                    create_index(&mut con, format!("idx:{}", &subreddit), format!("subreddit:{}:post:", &subreddit)).await?;
                }

                // Fetch a page and update state
                let subreddit_state = subreddits.get_mut(&subreddit).unwrap();
                let fetched_up_to = &subreddit_state.fetched_up_to;
                subreddit_state.fetched_up_to = get_subreddit(
                    subreddit.clone(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(),
                    None, imgur_client.clone(), fetched_up_to.clone(), Some(1), &downloaders_client, subscriber.clone()).await?;

                subreddit_state.last_fetched = Some(get_epoch_ms()?);
                subreddit_state.pages_left -= 1;

                debug!("Subreddit has {} pages left", subreddit_state.pages_left);
                
                if subreddit_state.fetched_up_to.is_none() {
                    debug!("Subreddit has no more pages left: {}", &subreddit);
                    subreddit_state.pages_left = 0;
                }

                // If we've fetched all the pages, remove the subreddit from the list
                if subreddit_state.pages_left == 0 {
                    subreddits.remove(&subreddit);
                    con.set(&subreddit, get_epoch_ms()?).await?;
                    info!("Got custom subreddit: {:?}", &subreddit);
                    con.lrem("custom_subreddits_queue", 0, &subreddit).await?;
                }
            }

            sleep(Duration::from_millis(10)).await;
        }
    }

    let mut last_run = 0;
    loop {
        if last_run > (get_epoch_ms()? - 10000*60) { // Only download every 10 minutes, to avoid rate limiting (and also it's just not necessary)
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        last_run = get_epoch_ms()?;

        let db = mongodb_client.database("config");
        let coll = db.collection::<Document>("settings");

        let filter = doc! {"id": "subreddit_list".to_string()};
        let find_options = FindOptions::builder().build();
        let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

        let doc = cursor.try_next().await.unwrap().unwrap();
        let mut sfw_subreddits: Vec<&str> = doc.get_array("sfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();
        let mut nsfw_subreddits: Vec<&str> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();

        let mut subreddits = Vec::new();
        subreddits.append(&mut nsfw_subreddits);
        subreddits.append(&mut sfw_subreddits);

        for subreddit in subreddits {
            debug!("Getting subreddit: {:?}", subreddit);
            let last_updated = con.get(&subreddit).await.unwrap_or(0u64);
            debug!("{:?} was last updated at {:?}", &subreddit, last_updated);

            get_subreddit(subreddit.to_string(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None, imgur_client.clone(), None, None, &downloaders_client, subscriber.clone()).await?;
            let _:() = con.set(&subreddit, get_epoch_ms()?).await.unwrap();
            if !index_exists(&mut con, format!("idx:{}", subreddit)).await {
                create_index(&mut con, format!("idx:{}", subreddit), format!("subreddit:{}:post:", subreddit)).await?;
            }
            info!("Got subreddit: {:?}", subreddit);
        }

        info!("All subreddits updated");

    }
}


#[tokio::main]
async fn main() {
    tracing_subscriber::Registry::default()
    .with(sentry::integrations::tracing::layer().event_filter(|md| {
        let level_filter = match md.level() {
            &tracing::Level::ERROR => sentry::integrations::tracing::EventFilter::Event,
            &tracing::Level::WARN => sentry::integrations::tracing::EventFilter::Event,
            &tracing::Level::TRACE => sentry::integrations::tracing::EventFilter::Ignore,
            _ => sentry::integrations::tracing::EventFilter::Breadcrumb,
        };
        if md.target().contains("reddit_downloader") {
            return level_filter;
        } else {
            return sentry::integrations::tracing::EventFilter::Ignore;
        }
    }))
    .with(tracing_subscriber::fmt::layer().compact().with_ansi(false).with_filter(tracing_subscriber::filter::LevelFilter::DEBUG).with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
        if !meta.target().contains("reddit_downloader") {
            return false;
        };
        true
    })))
    .init();

    println!("Initialised tracing!");

    let _guard = sentry::init(("https://75873f85a862465795299365b603fbb5@us.sentry.io/4504774760660992", sentry::ClientOptions {
        release: sentry::release_name!(),
        traces_sample_rate: 1.0,
        ..Default::default()
    }));

    println!("Initialised sentry");

    let reddit_secret = env::var("REDDIT_TOKEN").expect("REDDIT_TOKEN not set");
    let reddit_client = env::var("REDDIT_CLIENT_ID").expect("REDDIT_CLIENT_ID not set");

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("reddit_secret".to_string(), ConfigValue::String(reddit_secret.to_string())),
        ("reddit_client".to_string(), ConfigValue::String(reddit_client.to_string())),
    ]);

    let data = Arc::new(Mutex::new(contents));

    info!("Starting loops");
    match download_loop(data).await {
        Ok(_) => {
            info!("Finished");
        },
        Err(e) => {
            sentry::integrations::anyhow::capture_anyhow(&e);
            error!("Error: {:?}", e);
        }
    };
}