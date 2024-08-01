#[macro_use]
extern crate lazy_static;

use futures::lock::Mutex;
use futures::StreamExt;
use indexmap::IndexSet;
use itertools::Itertools;
use ordermap::OrderMap;
use post_subscriber::SubscriberClient;
use sentry::{Hub, SentryFutureExt};
use std::collections::HashMap;
use std::iter::{self, FromIterator};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::tokio_serde::formats::Bincode;
use tokio::sync::RwLock;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use truncrate::*;

use anyhow::{anyhow, Context};

use redis::AsyncCommands;

use futures_util::TryStreamExt;
use reqwest::header::{HeaderMap, USER_AGENT};
use std::env;

use mongodb::bson::{doc, Document};
use mongodb::options::{ClientOptions, FindOptions};
use serde_json::Value::Null;

use tarpc::{client, context, serde_transport::Transport};

mod downloaders;

lazy_static! {
    static ref MULTI_LOCK: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    static ref REDDIT_LIMITER: downloaders::client::Limiter =
        downloaders::client::Limiter::new(Some(100));
}

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
    _value: HashMap<String, ConfigValue<'a>>,
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
    /// Whether the subreddit should never stop being fetched.
    always_fetch: bool,
    /// Subreddit post list state
    post_list: SubredditPostList,
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
            ("timestamp".to_string(), post.timestamp.to_string()),
        ];

        if post.embed_url.is_some() {
            tmp.push(("embed_url".to_string(), post.embed_url.unwrap()));
        };

        tmp
    }
}

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> Result<u64, anyhow::Error> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
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
async fn request_access_token(
    reddit_client: String,
    reddit_secret: String,
    web_client: &reqwest::Client,
    device_id: Option<String>,
) -> Result<(String, u64), anyhow::Error> {
    let post_data = match device_id {
        Some(x) => format!(
            "grant_type=https://oauth.reddit.com/grants/installed_client&\\device_id={}",
            x
        ),
        None => "grant_type=client_credentials".to_string(),
    };

    REDDIT_LIMITER.wait().await;
    REDDIT_LIMITER.update().await;

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
    let token = results
        .get("access_token")
        .expect("Reddit did not return access token")
        .to_string();
    let expires_in: u64 = results
        .get("expires_in")
        .expect("Reddit did not provide expires_in")
        .as_u64()
        .unwrap();
    let expires_at = get_epoch_ms()? + expires_in * 1000;

    debug!(
        "New token expires in {} seconds, or at {}",
        expires_in, expires_at
    );

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
async fn get_access_token(
    con: &mut redis::aio::MultiplexedConnection,
    reddit_client: String,
    reddit_secret: String,
    web_client: &reqwest::Client,
    device_id: Option<String>,
) -> Result<String, anyhow::Error> {
    // Return a reddit access token
    let token_name = match device_id.clone() {
        // The key to grab from the DB
        Some(x) => x,
        _ => "default".to_string(),
    };

    let token = con.hget("reddit_tokens", token_name.clone()).await;
    let token = match token {
        Ok(x) => x,
        Err(_) => {
            debug!("Requesting new access token, none exists");
            let token_results = request_access_token(
                reddit_client.clone(),
                reddit_secret.clone(),
                web_client,
                device_id.clone(),
            )
            .await?;
            let access_token = token_results.0;
            let expires_at = token_results.1;

            con.hset(
                "reddit_tokens",
                token_name.clone(),
                format!("{},{}", access_token, expires_at),
            )
            .await?;

            format!("{},{}", access_token, expires_at)
        }
    };

    debug!("Token raw: {}", token);

    let expires_at: u64 = token.split(",").collect::<Vec<&str>>()[1].parse()?;
    let mut access_token: String = token.split(",").collect::<Vec<&str>>()[0]
        .parse::<String>()?
        .replace('"', "");

    if expires_at < get_epoch_ms()? {
        debug!("Requesting new access token, current one expired");
        let token_results = request_access_token(
            reddit_client.clone(),
            reddit_secret.clone(),
            web_client,
            device_id.clone(),
        )
        .await?;
        access_token = token_results.0;
        let expires_at = token_results.1;

        con.hset(
            "reddit_tokens",
            token_name,
            format!("{},{}", access_token, expires_at),
        )
        .await?;
    }

    debug!("Reddit Token: {}", access_token.replace("\"", ""));
    return Ok(access_token);
}

#[derive(Debug)]
struct PostStatus {
    failed: bool,
    finished: bool,
}

#[derive(Clone, Debug)]
struct SubredditPostList {
    subreddit: String,
    posts: Arc<RwLock<OrderMap<String, PostStatus>>>,
    existing_posts: Arc<IndexSet<String>>,
    con: redis::aio::MultiplexedConnection,
}

impl SubredditPostList {
    fn new(subreddit: String, con: redis::aio::MultiplexedConnection) -> Self {
        Self {
            subreddit,
            posts: Arc::new(RwLock::new(OrderMap::new())),
            existing_posts: Arc::new(IndexSet::new()),
            con,
        }
    }

    // Wait for all posts to be finished processing - means if the subreddit is being fetched again before the previous run has finished, it will wait for the previous run to finish before starting the new one
    // This is to stop ordering issues
    // When it's finished, it will reset its state so the next run can start
    #[tracing::instrument(skip(self, existing_posts))]
    async fn wait_all_finished_and_reset(&mut self, existing_posts: IndexSet<String>) {
        self.existing_posts = Arc::new(existing_posts);
    }

    async fn add_post(&self, id: &str, finished: bool) {
        let mut posts = self.posts.write().await;
        let post = PostStatus {
            failed: false,
            finished,
        };
        posts.insert(id.to_string(), post);
    }

    async fn set_failed(&self, id: &str) {
        let mut posts = self.posts.write().await;
        let post = posts.get_mut(id).unwrap();
        (*post).failed = true;
    }

    // Set given post ID as finished processing and update vector of finished posts in Redis
    #[tracing::instrument(skip(self))]
    async fn set_finished(&self, id: &str) -> Result<(), anyhow::Error> {
        debug!("Setting post as finished: {:?}", id);
        let mut posts = self.posts.write().await;
        debug!("Acquired mutex for setting post as finished: {:?}", id);
        match posts.get_mut(id) {
            None => {
                posts.insert(
                    id.to_string(),
                    PostStatus {
                        failed: false,
                        finished: true,
                    },
                );
            }
            Some(x) => {
                (*x).finished = true;
                (*x).failed = false;
            }
        };

        let existing_posts: IndexSet<&String> = (*self.existing_posts).iter().collect();
        // Generate list of finished posts including existing posts
        let finished_posts: Vec<&String> = posts
            .iter()
            .filter(|x| x.1.finished)
            .map(|x| x.0)
            .collect::<IndexSet<&String>>()
            .union(&existing_posts)
            .map(|x| *x)
            .collect();

        if finished_posts.len() != 0 {
            let parent_span = sentry::configure_scope(|scope| scope.get_span());
            let span: sentry::TransactionOrSpan = match &parent_span {
                Some(parent) => parent
                    .start_child("redis_push", "pushing new post list to redis")
                    .into(),
                None => {
                    let ctx = sentry::TransactionContext::new(
                        "redis_push",
                        "pushing new post list to redis",
                    );
                    sentry::start_transaction(ctx).into()
                }
            };
            let mut con = self.con.clone();
            redis::pipe()
                .atomic()
                .del::<String>(format!("subreddit:{}:posts", self.subreddit))
                .rpush::<String, Vec<&String>>(
                    format!("subreddit:{}:posts", self.subreddit),
                    finished_posts,
                )
                .query_async::<redis::aio::MultiplexedConnection, ()>(&mut con)
                .await?;

            span.finish();
        };

        debug!("Finished setting post as finished: {:?}", id);
        Ok(())
    }

    async fn exists(&self, id: &str) -> bool {
        let posts = self.posts.read().await;
        posts.iter().any(|x| x.0 == id && !x.1.failed)
            || self.existing_posts.iter().any(|x| *x == id)
    }
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
#[tracing::instrument(skip(
    con,
    web_client,
    downloaders_client,
    reddit_client,
    reddit_secret,
    device_id,
    subscriber,
    post_list
))]
async fn get_subreddit(
    subreddit: String,
    con: &mut redis::aio::MultiplexedConnection,
    web_client: &reqwest::Client,
    reddit_client: String,
    reddit_secret: String,
    device_id: Option<String>,
    mut after: Option<String>,
    pages: Option<u8>,
    downloaders_client: downloaders::client::Client<'static>,
    subscriber: SubscriberClient,
    mut post_list: SubredditPostList,
) -> Result<Option<String>, anyhow::Error> {
    // Get the top 1000 most recent posts and store them in the DB
    debug!("Fetching subreddit: {:?}", subreddit);

    let access_token = get_access_token(
        con,
        reddit_client.clone(),
        reddit_secret.clone(),
        web_client,
        device_id,
    )
    .await?;

    let url_base = format!(
        "https://oauth.reddit.com/r/{}/hot.json?limit=100",
        subreddit
    );

    let existing_posts: Vec<String> = redis::cmd("LRANGE")
        .arg(format!("subreddit:{}:posts", subreddit.clone()))
        .arg(0i64)
        .arg(-1i64)
        .query_async(con)
        .await
        .context("Getting existing posts")?;

    post_list
        .wait_all_finished_and_reset(existing_posts.into_iter().collect())
        .await;

    let subreddit = Arc::new(subreddit);

    debug!(
        "get_subreddit called with subreddit: {}, after: {:?}",
        subreddit, after
    );

    // Fetch pages of posts
    for x in 0..pages.unwrap_or(10) {
        debug!("Making {}th request to reddit", x);

        if (after.is_none() || after == Some("".into()) || after == Some("null".into())) && x != 0 {
            debug!("No more posts to fetch");
            break;
        }

        let url = match &after {
            Some(x) => {
                format!("{}&after={}", url_base, x.clone())
            }
            None => url_base.clone(),
        };

        debug!("URL: {}", url);

        // Set headers to tell Reddit who we are
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            format!(
                "Discord:RSlash:v{} (by /u/murrax2)",
                env!("CARGO_PKG_VERSION")
            )
            .parse()?,
        );
        headers.insert(
            "Authorization",
            format!("bearer {}", access_token.clone()).parse()?,
        );

        debug!("{}", url);
        debug!("{:?}", headers);

        REDDIT_LIMITER.wait().await;
        REDDIT_LIMITER.update().await;

        let res = web_client.get(&url).headers(headers).send().await?;

        debug!("Finished request");
        debug!("Response Headers: {:?}", res.headers());

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
            Some(x) => {
                after = x.get("after").map(|x| x.to_string().replace('"', ""));
                match x.get("children") {
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
                        let txt = format!(
                            "Failed to get field `children` in reddit response: {}",
                            text
                        );
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                        return Ok(after);
                    }
                }
            }
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

        let mut posts: Vec<
            Result<(usize, serde_json::Map<String, serde_json::Value>), anyhow::Error>,
        > = Vec::new();

        let mut count = 0;
        for post in results {
            posts.push(Ok((
                count,
                post["data"]
                    .as_object()
                    .ok_or(anyhow!("Reddit response json not object"))?
                    .to_owned(),
            )));
            count += 1;
        }

        let stream = futures::stream::iter(posts);

        let parent_span = sentry::configure_scope(|scope| scope.get_span());

        stream.for_each_concurrent(5, |result| {
            // Clone Arcs so they can be moved into the async block and each thread gets its own reference
            let mut con = con.clone();
            let subreddit = Arc::clone(&subreddit);
            let subscriber = subscriber.clone();
            let post_list = post_list.clone();
            let downloaders_client = downloaders_client.clone();

            // Create a new transaction as an independent continuation
            let ctx = sentry::TransactionContext::continue_from_span(
                "process subreddit post",
                "process_post",
                parent_span.clone(),
            );

            async move {
                let transaction = sentry::start_transaction(ctx);
                let (_, post) = match result {
                    Ok(x) => x,
                    Err(x) => {
                        let txt = format!("Failed to get post: {}", x);
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                        return;
                    }
                };
                debug!("{:?} - {:?}", post["title"], post["url"]);

                // Redis key for this post
                let key: String = format!("subreddit:{}:post:{}", post["subreddit"].to_string().replace("\"", ""), &post["id"].to_string().replace('"', ""));

                let exists = post_list.exists(&key).await;

                // If the post has been removed by a moderator, remove it from the DB and skip it
                if post.get("removed_by_category").unwrap_or(&Null) != &Null || post["author"].to_string().replace('"', "") == "[deleted]" {
                    debug!("Post is deleted");
                    if exists {
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

                if let Ok(media_deleted) = con.sismember("media_deleted_posts".to_string(), format!("{}:{}", subreddit, post["id"])).await {
                    if media_deleted {
                        debug!("Post is marked as having deleted media");
                        return;
                    }
                } else {
                    let txt = "Failed to check if post has deleted media";
                    warn!("{}", txt);
                    sentry::capture_message(&txt, sentry::Level::Warning);
                }

                if exists {
                    debug!("Post already exists in DB");
                    post_list.add_post(&key, true).await;

                    // Update post's score with new score
                    match con.hset::<&str, &str, i64, redis::Value>(&key, "score", post["score"].as_i64().unwrap_or(0)).await {
                        Ok(_) => {},
                        Err(x) => {
                            let txt = format!("Failed to update score in redis: {}", x);
                            warn!("{}", txt);
                            sentry::capture_message(&txt, sentry::Level::Warning);
                        }
                    };
                } else {
                    debug!("Post does not exist in DB");

                    tokio::spawn(async move {
                        let mut url = post["url"].to_string().replace('"', "");
                        if url.contains("v.redd.it") {
                            url = post["media"]["reddit_video"]["dash_url"].to_string().replace('"', "")
                        };

                        post_list.add_post(&key, false).await;

                        // If URL is already embeddable, no further processing is needed
                        if (url.ends_with(".gif") || url.ends_with(".png") || url.ends_with(".jpg") || url.ends_with(".jpeg")) && !url.contains("redgifs.com") {
                            debug!("URL is embeddable");
                        } else if url.ends_with(".mp4") || url.contains("imgur.com") || url.contains("redgifs.com") || url.contains(".mpd") {
                            debug!("URL is not embeddable, but we have the ability to turn it into one");

                            let mut res = match downloaders_client.request(&url, &post["id"].to_string().replace('"', "")).await {
                                Ok(x) => {
                                    debug!("Downloaded media: {}", x);
                                    x
                                },
                                Err(x) => {
                                    if x.root_cause().to_string() == "Deleted" {
                                        // If the post exists but the media it links to has been deleted, mark it as such so we don't try download it again
                                        match con.sadd::<String, String, ()>("media_deleted_posts".to_string(), format!("{}:{}", subreddit, post["id"])).await {
                                            Ok(_) => {},
                                            Err(x) => {
                                                let txt = format!("Failed to add post to media_deleted_posts: {}", x);
                                                warn!("{}", txt);
                                                sentry::capture_message(&txt, sentry::Level::Warning);
                                            }
                                        };
                                        return;
                                    } else {
                                        let txt = format!("Failed to download media with post URL: {}, URL: {}, Error: {}, root cause: {}", post["permalink"], &url, x, x.root_cause());
                                        warn!("{}", txt);
                                        sentry::capture_message(&txt, sentry::Level::Warning);
                                        post_list.set_failed(&key).await;
                                        return;
                                    }
                                }
                            };

                            if !res.starts_with("http") {
                                res = format!("https://r-slash.b-cdn.net/gifs/{}", res);
                            }

                            url = res;
                        } else {
                            debug!("URL is not embeddable, and we do not have the ability to turn it into one.");
                            return;
                        }

                        let timestamp = post["created_utc"].as_f64().unwrap() as u64;
                        let mut title = post["title"].to_string().replace('"', "").replace("&amp;", "&");
                        // Truncate title length to 256 chars (Discord embed title limit)
                        title = (*title.as_str()).truncate_to_boundary(256).to_string();

                        let post_object = Post {
                            score: post["score"].as_i64().unwrap_or(0),
                            url: format!("https://reddit.com{}", post["permalink"].to_string().replace('"', "")),
                            title: title,
                            embed_url: Some(url),
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

                        match post_list.set_finished(&key).await {
                            Ok(_) => {},
                            Err(x) => {
                                let txt = format!("Failed to set post as finished: {}", x);
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
                    });
                }
                transaction.finish();
            }.bind_hub(Hub::new_from_top(Hub::current()))
        }
        ).await;
    }

    return Ok(after);
}

// Check if RediSearch index exists
#[tracing::instrument(skip(con))]
async fn index_exists(con: &mut redis::aio::MultiplexedConnection, index: String) -> bool {
    match redis::cmd("FT.INFO")
        .arg(index)
        .query_async::<redis::aio::MultiplexedConnection, redis::Value>(con)
        .await
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

// Create RediSearch index for subreddit
#[tracing::instrument(skip(con))]
async fn create_index(
    con: &mut redis::aio::MultiplexedConnection,
    index: String,
    prefix: String,
) -> Result<(), anyhow::Error> {
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
        .query_async(con)
        .await?)
}

/// Start the downloading loop
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn download_loop<'a>(
    data: Arc<Mutex<HashMap<String, ConfigValue<'_>>>>,
) -> Result<(), anyhow::Error> {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis");

    let web_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("Discord:RSlash:v1.0.1 (by /u/murrax2)")
        .build()
        .expect("Failed to build client");

    let data_lock = data.lock().await;
    let reddit_secret = match data_lock.get("reddit_secret").unwrap() {
        ConfigValue::String(x) => x,
        _ => panic!("Failed to get reddit_secret"),
    }
    .clone();

    let reddit_client = match data_lock.get("reddit_client").unwrap() {
        ConfigValue::String(x) => x,
        _ => panic!("Failed to get reddit_client"),
    }
    .clone();

    let do_custom = env::var("DO_CUSTOM").expect("DO_CUSTOM not set");

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some("Downloader".to_string());

    let mongodb_client =
        mongodb::Client::with_options(client_options).expect("failed to connect to mongodb");

    let imgur_client_id = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");

    let downloaders_client =
        downloaders::client::Client::new("/data/media", Some(imgur_client_id), None).await?;

    let reconnect_opts = ReconnectOptions::new()
        .with_exit_if_first_connect_fails(false)
        .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
    let tcp_stream = StubbornTcpStream::connect_with_options(
        "post-subscriber.discord-bot-shared.svc.cluster.local:50051",
        reconnect_opts,
    )
    .await?;
    let transport = Transport::from((tcp_stream, Bincode::default()));

    let subscriber =
        post_subscriber::SubscriberClient::new(client::Config::default(), transport).spawn();

    debug!("Starting subreddit loop");
    let mut subreddits: HashMap<String, SubredditState> = HashMap::new();
    if do_custom != "true".to_string() {
        let db = mongodb_client.database("config");
        let coll = db.collection::<Document>("settings");

        let filter = doc! {"id": "subreddit_list".to_string()};
        let find_options = FindOptions::builder().build();
        let mut cursor = coll
            .find(filter.clone(), find_options.clone())
            .await
            .unwrap();

        let doc = cursor.try_next().await.unwrap().unwrap();
        let mut sfw_subreddits: Vec<&str> = doc
            .get_array("sfw")
            .unwrap()
            .into_iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        let mut nsfw_subreddits: Vec<&str> = doc
            .get_array("nsfw")
            .unwrap()
            .into_iter()
            .map(|x| x.as_str().unwrap())
            .collect();

        let mut subreddits_vec = Vec::new();
        subreddits_vec.append(&mut nsfw_subreddits);
        subreddits_vec.append(&mut sfw_subreddits);

        for subreddit in subreddits_vec {
            subreddits.insert(
                subreddit.to_string(),
                SubredditState {
                    name: subreddit.to_string(),
                    fetched_up_to: None,
                    last_fetched: None,
                    pages_left: 10,
                    always_fetch: true,
                    post_list: SubredditPostList::new(subreddit.to_string(), con.clone()),
                },
            );
        }
    }
    loop {
        // Populate subreddits with any new subreddit requests
        let custom_subreddits: Vec<String> = redis::cmd("LRANGE")
            .arg("custom_subreddits_queue")
            .arg(0i64)
            .arg(-1i64)
            .query_async(&mut con)
            .await
            .unwrap_or(Vec::new());
        for subreddit in custom_subreddits {
            if !subreddits.contains_key(&subreddit) {
                let last_fetched: Option<u64> = con.get(&subreddit).await?;
                subreddits.insert(
                    subreddit.clone(),
                    SubredditState {
                        name: subreddit.clone(),
                        fetched_up_to: None,
                        last_fetched,
                        pages_left: 10,
                        always_fetch: false,
                        post_list: SubredditPostList::new(subreddit.clone(), con.clone()),
                    },
                );
            }
        }

        match subscriber.watched_subreddits(context::current()).await? {
            Ok(watched_subreddits) => {
                for subreddit in watched_subreddits {
                    if !subreddits.contains_key(&subreddit) {
                        let last_fetched: Option<u64> = con.get(&subreddit).await?;
                        subreddits.insert(
                            subreddit.clone(),
                            SubredditState {
                                name: subreddit.clone(),
                                fetched_up_to: None,
                                last_fetched,
                                pages_left: 10,
                                always_fetch: false,
                                post_list: SubredditPostList::new(subreddit.clone(), con.clone()),
                            },
                        );
                    }
                }
            }
            Err(x) => {
                let txt = format!("Failed to get watched subreddits: {}", x);
                error!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Error);
            }
        }

        let mut next_subreddit = None;

        // If there's a subreddit that hasn't been fetched yet, make that the next one
        for subreddit in subreddits.values() {
            if subreddit.last_fetched.is_none() {
                next_subreddit = Some(subreddit.name.clone());
                debug!(
                    "Found subreddit that hasn't been fetched yet: {}",
                    subreddit.name
                );
                break;
            }
        }

        // If there's no subreddit that hasn't been fetched yet, find the one fetched least recently and make that the next one
        match next_subreddit {
            Some(_) => {}
            None => {
                let mut min = None;
                // Find the minimum last_fetched
                for subreddit in subreddits.values() {
                    match min {
                        Some(x) => {
                            if subreddit.last_fetched.unwrap() < x {
                                min = subreddit.last_fetched;
                            }
                        }
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
                create_index(
                    &mut con,
                    format!("idx:{}", &subreddit),
                    format!("subreddit:{}:post:", &subreddit),
                )
                .await?;
            }

            // Fetch a page and update state
            let subreddit_state = subreddits.get_mut(&subreddit).unwrap();
            let fetched_up_to = &subreddit_state.fetched_up_to;
            subreddit_state.fetched_up_to = match get_subreddit(
                subreddit.clone(),
                &mut con,
                &web_client,
                reddit_client.clone(),
                reddit_secret.clone(),
                None,
                fetched_up_to.clone(),
                Some(1),
                downloaders_client.clone(),
                subscriber.clone(),
                subreddit_state.post_list.clone(),
            )
            .await
            {
                Ok(x) => x,
                Err(x) => {
                    let txt = format!("Failed to get subreddit: {}", x);
                    error!("{}", txt);
                    continue;
                }
            };

            subreddit_state.last_fetched = Some(get_epoch_ms()?);
            subreddit_state.pages_left -= 1;

            debug!("Subreddit has {} pages left", subreddit_state.pages_left);

            if subreddit_state.fetched_up_to.is_none() {
                debug!("Subreddit has no more pages left: {}", &subreddit);
                subreddit_state.pages_left = 0;
            }

            // If we've fetched all the pages, remove the subreddit from the list
            if subreddit_state.pages_left == 0 && !subreddit_state.always_fetch {
                subreddits.remove(&subreddit);
                con.set(&subreddit, get_epoch_ms()?).await?;
                info!("Got custom subreddit: {:?}", &subreddit);
                con.lrem("custom_subreddits_queue", 0, &subreddit).await?;
            } else if subreddit_state.pages_left == 0 {
                subreddit_state.pages_left = 10;
                subreddit_state.fetched_up_to = None;
                let mut posts: tokio::sync::RwLockWriteGuard<OrderMap<String, PostStatus>> =
                    subreddit_state.post_list.posts.write().await;
                (*posts).retain(|_, value| value.finished == false);
            }
        }

        sleep(Duration::from_millis(10)).await;
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
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
                    if !meta.target().contains("reddit_downloader") {
                        return false;
                    };
                    true
                })),
        )
        .init();

    println!("Initialised tracing!");

    let _guard = sentry::init((
        "https://75873f85a862465795299365b603fbb5@us.sentry.io/4504774760660992",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 1.0,
            ..Default::default()
        },
    ));

    println!("Initialised sentry");

    let reddit_secret = env::var("REDDIT_TOKEN").expect("REDDIT_TOKEN not set");
    let reddit_client = env::var("REDDIT_CLIENT_ID").expect("REDDIT_CLIENT_ID not set");

    let contents: HashMap<String, ConfigValue> = HashMap::from_iter([
        (
            "reddit_secret".to_string(),
            ConfigValue::String(reddit_secret.to_string()),
        ),
        (
            "reddit_client".to_string(),
            ConfigValue::String(reddit_client.to_string()),
        ),
    ]);

    let data = Arc::new(Mutex::new(contents));

    info!("Starting loops");
    match download_loop(data).await {
        Ok(_) => {
            info!("Finished");
        }
        Err(e) => {
            sentry::integrations::anyhow::capture_anyhow(&e);
            error!("Error: {:?}", e);
        }
    };
}
