use tracing::{debug, info, warn, error};
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use tokio::time::{sleep, Duration};
use futures::{lock::Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use truncrate::*;

use redis::AsyncCommands;

use reqwest::header::{USER_AGENT, HeaderMap};
use std::env;
use futures_util::TryStreamExt;

use mongodb::bson::{doc, Document};
use mongodb::options::{FindOptions};
use serde_json::Value::Null;


/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    Bool(bool),
    String(String),
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
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
    embed_url: String,
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
        vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url.to_string()),
            ("title".to_string(), post.title.to_string()),
            ("embed_url".to_string(), post.embed_url.to_string()),
            ("author".to_string(), post.author.to_string()),
            ("id".to_string(), post.id.to_string()),
            ("timestamp".to_string(), post.timestamp.to_string())
        ]
    }
}


/// Returns current milliseconds since the Epoch
#[tracing::instrument]
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

    let results: serde_json::Value = res.json().await?;
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
/// The [connection](redis::aio::Connection) to the redis DB.
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## device_id
/// If specified, will request an [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
#[tracing::instrument(skip(con, web_client))]
async fn get_access_token(con: &mut redis::aio::Connection, reddit_client: String, reddit_secret: String, web_client: &reqwest::Client, device_id: Option<String>) -> Result<String, anyhow::Error> { // Return a reddit access token
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
///
/// # Arguments
/// ## subreddit
/// A [String](String) representing the subreddit name, without the `r/`
/// ## con
/// A [Connection](redis::aio::Connection) to the Redis DB
/// ## web_client
/// The Reqwest [Client](reqwest::Client)
/// ## reddit_client
/// The Reddit client as [String](String)
/// ## reddit_secret
/// The Reddit access token as a [String](String)
/// ## device_id
/// None if a default subreddit, otherwise is the user's ID.
#[tracing::instrument(skip(con, web_client))]
async fn get_subreddit(subreddit: String, con: &mut redis::aio::Connection, web_client: &reqwest::Client, reddit_client: String, reddit_secret: String, device_id: Option<String>, gfycat_token: &mut OauthToken, imgur_client: String, mut after: Option<String>, pages: Option<u8>) -> Result<Option<String>, anyhow::Error> { // Get the top 1000 most recent posts and store them in the DB
    debug!("Placeholder: {:?}", subreddit);

    let access_token = get_access_token(con, reddit_client.clone(), reddit_secret.clone(), web_client, device_id).await?;

    let mut keys = Vec::new();
    let url_base = format!("https://oauth.reddit.com/r/{}/hot.json?limit=100", subreddit);
    let mut url = String::new();
    let mut existing_posts: Vec<String> = redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(-1i64).query_async(con).await?;
    debug!("Existing posts: {:?}", existing_posts);
    for x in 0..pages.unwrap_or(10) {
        debug!("Making {}th request to reddit", x);

        if url.len() == 0 {
            url = url_base.clone();
            match &after {
                Some(x) => {
                    url = format!("{}&after={}", url, x.clone());
                },
                None => {},
            }
        }

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, "Discord:RSlash:v1.0.1 (by /u/murrax2)".parse()?);
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
        let last_requested = get_epoch_ms()?;
        debug!("Got time");
        let text = match res.text().await {
            Ok(x) => x,
            Err(x) => {
                let txt = format!("Failed to get text from reddit: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        };
        debug!("Matched text");
        let results: serde_json::Value = match serde_json::from_str(&text) {
            Ok(x) => x,
            Err(_) => {
                let txt = format!("Failed to parse JSON from Reddit: {}", text);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok(after);
            }
        };

        let results = results.get("data").unwrap().get("children").unwrap().as_array().unwrap();

        if results.len() == 0 {
            debug!("No results");
            break;
        }

        after = Some(results[results.len() -1 ]["data"]["id"].as_str().unwrap().to_string());
        match &after {
            Some(x) => {
                url = format!("{}&after={}", url_base, x.clone());
            },
            None => {},
        }

        let mut posts = Vec::new();
        for post in results {
            // For gfycat get media/oembed/thumbnail_url, replace thumbs with giant, and remove size-restricted, and replace .gif with .mp4
            // If removed_by_category is present then post is removed
            // For imgur, get url, and replace extension with .mp4 (might not have any extension), if url has gallery in it, get url from media/oembed/thumbnail_url and replace with .mp4 again
            // For i.redd.it just take the L and use the url
            let post = post["data"].clone();
            debug!("{:?} - {:?}", post["title"], post["url"]);
            let mut url = post["url"].clone().to_string();

            if post.get("removed_by_category").unwrap_or(&Null) != &Null {
                debug!("Post removed by moderator");
                continue;
            }

            if post["author"].to_string().replace('"', "") == "[deleted]" {
                debug!("Post removed by author");
                continue;
            }

            if post["title"].to_string() == "Flipping a pancake" { // For some reason pinned images aren't marked as pinned on the api, and this one post is pinned but doesn't embed.
                continue;
            }

            if !existing_posts.contains(&format!("subreddit:{}:post:{}", subreddit.clone(), &post["id"].to_string().replace('"', ""))) {
                debug!("{}", format!("subreddit:{}:post:{}", subreddit.clone(), &post["id"].to_string().replace('"', "")));
                url = url.replace(".gifv", ".gif");

                if url.contains("gfycat") {
                    let id = url.split("/").last().unwrap().split(".").next().unwrap().replace('"', "");
                    debug!("{}", id);

                    if gfycat_token.expires_at < get_epoch_ms()? - 1000 {
                        match gfycat_token.refresh().await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to refresh gfycat token: {}", x);
                                error!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Error);
                                continue;
                            }
                        };
                    }

                    let auth = format!("Bearer {}", gfycat_token.token);

                    let res = match web_client
                        .get(format!("https://api.gfycat.com/v1/gfycats/{}", id))
                        .header("Authorization", auth)
                        .send()
                        .await {
                            Ok(x) => x,
                            Err(x) => {
                                let txt = format!("Failed to request from gfycat: {}", x);
                                error!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Error);
                                continue;
                            }
                    };

                    let results: serde_json::Value = res.json().await.unwrap();
                    let result = results.get("gfyItem");
                    match result {
                        Some(x) => {
                            url = x.get("gifUrl").unwrap().to_string();
                        },
                        None => {continue},
                    };

                } else if url.contains("imgur") && !url.contains(".gif") && !url.contains(".png") && !url.contains(".jpg") && !url.contains(".jpeg") {
                    let auth = format!("Client-ID {}", imgur_client);
                    let id = url.split("/").last().unwrap().split(".").next().unwrap().split("?").next().unwrap().replace('"', "");

                    let res = match web_client
                        .get(format!("https://api.imgur.com/3/image/{}", id))
                        .header("Authorization", auth)
                        .send()
                        .await {
                            Ok(x) => x,
                            Err(y) => {
                                let txt = format!("Failed to request from imgur: {}", y);
                                error!("{}", txt);
                                sentry::capture_message(&txt, sentry::Level::Error);
                                continue;
                            }
                    };

                    if res.status() != 200 {
                        debug!("{} while fetching from imgur", res.status());
                        continue
                    }
                    let results: serde_json::Value = res.json().await.unwrap();
                    url = results.get("data").unwrap().get("link").unwrap().to_string().replace(".gifv", ".gif").replace(".mp4", ".gif");


                } else if url.contains("i.redd.it") == false  && url.contains(".gif") == false  && url.contains(".jpg") == false  && url.contains(".png") == false  && url.contains("redgif") == false{
                    // URL is not embeddable, and we do not have the ability to turn it into one.
                    debug!("{} is not embeddable", url);
                    continue;
                }

            } else {
                debug!("Post is in cache, not fetching URL");
                continue;
            }

            url = url.replace('"', "");
            match web_client
                .head(&url)
                .send()
                .await {
                Ok(x) => {
                    let length = x.content_length().unwrap_or(0);
                    if length > 15000000 {
                        debug!("Content bigger than 15MB, skipping."); // Otherwise Discord takes too long to load the content
                        continue;
                    }
                },
                Err(y) => {
                    warn!("{}", y);
                    continue;
                }
            }



            debug!("Post Score: {:?}", post["score"]);

            let timestamp = post["created_utc"].as_f64().unwrap() as u64;

            let mut post_object = Post {
                score: post["score"].as_i64().unwrap_or(0),
                url: format!("https://reddit.com{}", post["permalink"].to_string().replace('"', "")),
                title: post["title"].to_string().replace('"', ""),
                embed_url: url.to_string().replace('"', ""),
                author: post["author"].to_string().replace('"', ""),
                id: post["id"].to_string().replace('"', ""),
                timestamp: timestamp,
            };

            post_object.title = (*post_object.title.as_str()).truncate_to_boundary(256).to_string();
            posts.push(post_object);
        }

        let mut new_post_count = 0;
        for post in posts.clone() {
            let key = format!("subreddit:{}:post:{}", subreddit.clone(), post.id.replace('"', ""));
            keys.push(key.clone());
            let value = Vec::from(post);
            con.hset_multiple(key, &value).await?;
            new_post_count += 1;
        }

        let mut old_posts = Vec::new();
        for post in existing_posts.clone() {
            if keys.contains(&post) {
                continue;
            } else {
                old_posts.push(post);
            }
        }

        debug!("Old Posts Keys Length: {:?}", old_posts.len());
        debug!("New Posts Keys Length: {:?}", keys.len());
        debug!("New Posts Found: {:?}", new_post_count);
        old_posts.append(&mut keys);

        let keys = old_posts;
        existing_posts = keys.clone();

        debug!("Total Keys Length: {}", keys.len());
        if keys.len() != 0 {
            con.del(format!("subreddit:{}:posts", subreddit)).await?;
            con.lpush(format!("subreddit:{}:posts", subreddit), keys.clone()).await?;
        }

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
async fn index_exists(con: &mut redis::aio::Connection, index: String) -> bool {
    match redis::cmd("FT.INFO").arg(index).query_async::<redis::aio::Connection, redis::Value>(con).await {
        Ok(_) => true,
        Err(_) => false,
    }
}


// Create RediSearch index for subreddit
#[tracing::instrument(skip(con))]
async fn create_index(con: &mut redis::aio::Connection, index: String, prefix: String) -> Result<(), anyhow::Error>{
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

#[derive(Debug)]
struct OauthToken {
    token: String,
    expires_at: u64,
    client_id: String,
    client_secret: String,
    url: String,
}

impl OauthToken {
    #[tracing::instrument]
    async fn new(client_id: String, client_secret: String, url: String) -> Result<OauthToken, Box<dyn std::error::Error>> {
        let web_client = reqwest::Client::new();

        let mut post_data = HashMap::new();
        post_data.insert("client_id", client_id.clone());
        post_data.insert("client_secret", client_secret.clone());
        post_data.insert("grant_type", "client_credentials".to_string());

        let res = web_client
        .post(&url)
        .json(&post_data)
        .send()
        .await?;

        let results: serde_json::Value = res.json().await?;

        Ok(OauthToken {
            token: results["access_token"].to_string(),
            expires_at: results["expires_in"].as_u64().unwrap() + get_epoch_ms()?,
            client_id,
            client_secret,
            url,
        })
    }

    #[tracing::instrument]
    async fn refresh(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let new = OauthToken::new(self.client_id.clone(), self.client_secret.clone(), self.url.clone()).await?;
        self.token = new.token;
        self.expires_at = new.expires_at;

        Ok(())
    }
}

/// Start the downloading loop
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn download_loop(data: Arc<Mutex<HashMap<String, ConfigValue>>>) -> Result<(), anyhow::Error>{
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_async_connection().await.expect("Can't connect to redis");

    let web_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
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

    let gfycat_client = env::var("GFYCAT_CLIENT").expect("GFYCAT_CLIENT not set");
    let gfycat_secret = env::var("GFYCAT_SECRET").expect("GFYCAT_SECRET not set");
    let imgur_client = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");
    let do_custom = env::var("DO_CUSTOM").expect("DO_CUSTOM not set");
    let mut gfycat_token = OauthToken::new(gfycat_client, gfycat_secret, "https://api.gfycat.com/v1/oauth/token".to_string()).await.expect("Failed to get gfycat token");

    let mut client_options = mongodb::options::ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.expect("Failed to parse client options");
    client_options.app_name = Some("Downloader".to_string());

    let mongodb_client = mongodb::Client::with_options(client_options).expect("failed to connect to mongodb");

    if do_custom == "true".to_string() {
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
                if !index_exists(&mut con, format!("idx:{}", &subreddit)).await {
                    create_index(&mut con, format!("idx:{}", &subreddit), format!("subreddit:{}:post:", &subreddit)).await?;
                }

                // Fetch a page and update state
                let mut subreddit_state = subreddits.get_mut(&subreddit).unwrap();
                let fetched_up_to = &subreddit_state.fetched_up_to;
                subreddit_state.fetched_up_to = get_subreddit(
                    subreddit.clone(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(),
                    None, &mut gfycat_token, imgur_client.clone(), fetched_up_to.clone(), Some(1)).await?;

                if subreddit_state.fetched_up_to.is_none() {
                    error!("Failed to get subreddit, after is None: {}", &subreddit);
                    subreddit_state.pages_left = 1;
                }
                subreddit_state.last_fetched = Some(get_epoch_ms()?);
                debug!("Subreddit has {} pages left", subreddit_state.pages_left);
                subreddit_state.pages_left -= 1;

                // If we've fetched all the pages, remove the subreddit from the list
                if subreddit_state.pages_left == 0 {
                    subreddits.remove(&subreddit);
                    let _:() = con.set(&subreddit, get_epoch_ms()?).await.unwrap();
                    info!("Got custom subreddit: {:?}", &subreddit);
                    let _:() = con.lrem("custom_subreddits_queue", 0, &subreddit).await.unwrap();
                }
            }

            sleep(Duration::from_millis(100)).await;
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
        subreddits.append(&mut sfw_subreddits);
        subreddits.append(&mut nsfw_subreddits);

        for subreddit in subreddits {
            debug!("Getting subreddit: {:?}", subreddit);
            let last_updated = con.get(&subreddit).await.unwrap_or(0u64);
            debug!("{:?} was last updated at {:?}", &subreddit, last_updated);

            get_subreddit(subreddit.clone().to_string(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None, &mut gfycat_token, imgur_client.clone(), None, None).await?;
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
    .with(sentry::integrations::tracing::layer())
    .with(tracing_subscriber::fmt::layer())
    .init();

    println!("Initialised tracing");

    let _guard = sentry::init(("https://75873f85a862465795299365b603fbb5@o4504774745718784.ingest.sentry.io/4504774760660992", sentry::ClientOptions {
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