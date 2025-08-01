#[macro_use]
extern crate lazy_static;

use futures::lock::Mutex;
use post_subscriber::SubscriberClient;
use sentry::integrations::anyhow::capture_anyhow;
use std::collections::HashMap;
use std::iter::{self, FromIterator};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::tokio_serde::formats::Bincode;
use tokio::time::{Duration, sleep};

use tracing::{debug, error, info, trace, warn};
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use truncrate::*;

use anyhow::{Context, Error, anyhow};

use redis::{AsyncCommands, FromRedisValue, RedisResult, Value};

use futures_util::TryStreamExt;
use mongodb::bson::{Document, doc};
use mongodb::options::ClientOptions;
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{logs, trace};
use reqwest::header;
use reqwest::header::{HeaderMap, USER_AGENT};
use serde_json::Value::Null;
use std::env;

use crate::helpers::{
    Embeddability, extract_raw_embed_urls_from_post, media_url_embeddability, process_post_metadata,
};
use rslash_common::access_tokens::get_reddit_access_token;
use rslash_common::{initialise_observability, span_filter};
use tarpc::{client, context, serde_transport::Transport};

mod downloaders;
mod helpers;

lazy_static! {
    static ref MULTI_LOCK: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
    static ref REDDIT_LIMITER: rslash_common::Limiter =
        rslash_common::Limiter::new(None, "reddit".to_string());
}

/// Represents a value stored in a [ConfigStruct]
pub enum ConfigValue {
    U64(u64),
    Bool(bool),
    String(String),
    DownloaderClient(downloaders::client::Client),
    SubscriberClient(SubscriberClient),
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>,
}

#[derive(Debug)]
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
}

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> Result<u64, Error> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct NewPost {
    redis_key: String,
    id: String,
    embed_urls: Vec<String>,
    score: i64,
    title: String,
    url: String,
    timestamp: u64,
    needs_processing: bool,
    author: String,
}

impl From<NewPost> for Vec<(String, String)> {
    #[tracing::instrument]
    fn from(post: NewPost) -> Vec<(String, String)> {
        vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url),
            ("title".to_string(), post.title),
            ("author".to_string(), post.author),
            ("id".to_string(), post.id),
            ("timestamp".to_string(), post.timestamp.to_string()),
            ("embed_url".to_string(), post.embed_urls.join(",")),
        ]
    }
}

impl From<&NewPost> for Vec<(String, String)> {
    #[tracing::instrument]
    fn from(post: &NewPost) -> Vec<(String, String)> {
        vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url.to_string()),
            ("title".to_string(), post.title.to_string()),
            ("author".to_string(), post.author.to_string()),
            ("id".to_string(), post.id.to_string()),
            ("timestamp".to_string(), post.timestamp.to_string()),
            ("embed_url".to_string(), post.embed_urls.join(",")),
        ]
    }
}

impl Into<Post> for NewPost {
    fn into(self) -> Post {
        Post::New(self)
    }
}

impl Into<Post> for String {
    fn into(self) -> Post {
        Post::Existing(self)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Post {
    /// Contains redis key
    Existing(String),
    New(NewPost),
}

impl FromRedisValue for Post {
    fn from_redis_value(v: &Value) -> RedisResult<Self> {
        match v {
            Value::SimpleString(x) => Ok(Post::Existing(x.clone().replace('"', ""))),
            Value::BulkString(x) => Ok(Post::Existing(
                String::from_utf8_lossy(x).to_string().replace('"', ""),
            )),
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::ParseError,
                "redis value not supported",
                format!("Redis value was: {:?}", v),
            ))),
        }
    }

    fn from_owned_redis_value(v: Value) -> RedisResult<Self> {
        match v {
            Value::SimpleString(x) => Ok(Post::Existing(x.clone().replace('"', ""))),
            Value::BulkString(x) => Ok(Post::Existing(
                String::from_utf8_lossy(&x).to_string().replace('"', ""),
            )),
            _ => Err(redis::RedisError::from((
                redis::ErrorKind::ParseError,
                "redis value not supported",
                format!("Redis value was: {:?}", v),
            ))),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum SubredditExists {
    Exists,
    DoesntExist,
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
    downloaders_client: downloaders::client::Client,
    subscriber: SubscriberClient,
) -> Result<(SubredditExists, Option<String>), Error> {
    trace!("Fetching subreddit: {}, after: {:?}", subreddit, after);

    let subreddit = subreddit.to_lowercase();

    let access_token = get_reddit_access_token(
        con.clone(),
        reddit_client.clone(),
        reddit_secret.clone(),
        Some(web_client),
        device_id,
    )
    .await?;

    let url_base = format!(
        "https://oauth.reddit.com/r/{}/hot.json?limit=100",
        subreddit
    );

    let mut existing_posts: Vec<Post> = redis::cmd("LRANGE")
        .arg(format!("subreddit:{}:posts", subreddit.clone()))
        .arg(0i64)
        .arg(-1i64)
        .query_async(con)
        .await
        .context("Getting existing posts")?;

    let mut post_list: Vec<Post> = Vec::new();
    post_list.reserve(existing_posts.len() + 1000);

    let mut subreddit_name = None;

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

        let res = web_client.get(&url).headers(headers).send().await?;

        REDDIT_LIMITER
            .update_headers(res.headers(), res.status())
            .await?;

        debug!("Finished request");
        debug!("Response Headers: {:?}", res.headers());

        // Process text response from Reddit
        let text = match res.text().await {
            Ok(x) => x,
            Err(x) => {
                let txt = format!("Failed to get text from reddit: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
                return Ok((SubredditExists::Exists, after));
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
                return Ok((SubredditExists::Exists, after));
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
                            let txt = format!(
                                "Failed to convert field `children` in reddit response to array: {}",
                                text
                            );
                            warn!("{}", txt);
                            sentry::capture_message(&txt, sentry::Level::Warning);
                            return Ok((SubredditExists::Exists, after));
                        }
                    },
                    None => {
                        let txt = format!(
                            "Failed to get field `children` in reddit response: {}",
                            text
                        );
                        warn!("{}", txt);
                        sentry::capture_message(&txt, sentry::Level::Warning);
                        return Ok((SubredditExists::Exists, after));
                    }
                }
            }
            None => {
                let txt = format!("Failed to get field `data` in reddit response: {}", text);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);

                if let Some(err) = results.get("error") {
                    if let Some(err_code) = err.as_u64() {
                        if err_code == 404 {
                            debug!("Subreddit doesn't exist.");
                            return Ok((SubredditExists::DoesntExist, None));
                        } else if err_code == 403 {
                            if let Some(err_msg) = results.get("reason") {
                                if err_msg == "private" {
                                    debug!("Subreddit is private.");
                                    return Ok((SubredditExists::DoesntExist, None));
                                }
                            }
                        }
                    }
                }
                debug!("Subreddit exists");
                return Ok((SubredditExists::Exists, after));
            }
        };

        if results.len() == 0 {
            debug!("No results");
            break;
        }

        let mut posts: Vec<Result<(usize, serde_json::Map<String, serde_json::Value>), Error>> =
            Vec::new();

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

        let parent_span = sentry::configure_scope(|scope| scope.get_span());
        for post in posts {
            match process_post_metadata(
                parent_span.clone(),
                post,
                &mut subreddit_name,
                &mut con.clone(),
                subscriber.clone(),
                &mut existing_posts,
                &mut post_list,
                &subreddit,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => {
                    let txt = format!("Failed to process post metadata: {}", e);
                    warn!("{}", txt);
                }
            }
        }
    }

    // Append post_list with existing posts that aren't in the current fetch
    let existing_posts: Vec<Post> = existing_posts
        .into_iter()
        .filter(|x| !post_list.contains(x))
        .collect();
    post_list = post_list
        .into_iter()
        .chain(existing_posts.into_iter())
        .collect();

    if let Some(subreddit_name) = subreddit_name {
        // Add to processing queue
        downloaders_client
            .queue_subreddit_for_processing(&subreddit_name, post_list)
            .await?;
    }

    Ok((SubredditExists::Exists, after))
}

/// Start the downloading loop
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn download_loop<'a>(data: Arc<Mutex<HashMap<String, ConfigValue>>>) -> Result<(), Error> {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/")?;
    let mut con = db_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis");

    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        header::COOKIE,
        header::HeaderValue::from_static("_options={%22pref_gated_sr_optin%22:true}"),
    );
    let web_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("Discord:RSlash:v1.0.1 (by /u/murrax2)")
        .default_headers(default_headers)
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

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await?;
    client_options.app_name = Some("Downloader".to_string());

    let mongodb_client =
        mongodb::Client::with_options(client_options).expect("failed to connect to mongodb");

    let imgur_client_id = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");

    let reconnect_opts = ReconnectOptions::new()
        .with_exit_if_first_connect_fails(false)
        .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
    let tcp_stream = StubbornTcpStream::connect_with_options(
        "post-subscriber.discord-bot-shared.svc.cluster.local:50051",
        reconnect_opts,
    )
    .await?;
    let transport = Transport::from((tcp_stream, Bincode::default()));

    let subscriber = SubscriberClient::new(client::Config::default(), transport).spawn();

    let downloaders_client = downloaders::client::Client::new(
        "/data/media".to_string(),
        Some(imgur_client_id),
        con.clone(),
        subscriber.clone(),
    )
    .await?;

    debug!("Starting subreddit loop");
    let mut subreddits: HashMap<String, SubredditState> = HashMap::new();
    if do_custom != "true".to_string() {
        let db = mongodb_client.database("config");
        let coll = db.collection::<Document>("settings");

        let filter = doc! {"id": "subreddit_list".to_string()};
        let mut cursor = coll.find(filter.clone()).await?;

        let doc = cursor.try_next().await?.unwrap();
        let mut sfw_subreddits: Vec<&str> = doc
            .get_array("sfw")?
            .into_iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        let mut nsfw_subreddits: Vec<&str> = doc
            .get_array("nsfw")?
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
            let subreddit = next_subreddit.unwrap();
            if !downloaders_client.is_finished(&subreddit).await {
                trace!("Skipping {} as it is already being processed", subreddit);
                continue;
            }
            info!("Fetching subreddit {}", &subreddit);
            if !helpers::index_exists(&mut con, format!("idx:{}", &subreddit)).await {
                helpers::create_index(
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
            )
            .await
            {
                Ok(x) => {
                    // If the subreddit doesn't exist/isn't accessible, remove it from the list of subreddits to process
                    if x.0 == SubredditExists::DoesntExist {
                        info!("Subreddit doesn't exist, removing from list");
                        subreddits.remove(&subreddit);
                        let _: () = con.set(&subreddit, get_epoch_ms()?).await?;
                        let _: () = con.lrem("custom_subreddits_queue", 0, &subreddit).await?;
                        subscriber
                            .remove_subreddit(context::current(), subreddit.clone())
                            .await?
                            .map_err(|e| Error::msg(e))?;
                        continue;
                    };
                    x.1
                }
                Err(x) => {
                    let txt = format!("Failed to get subreddit: {:?}", x);
                    error!("{}", txt);
                    continue;
                }
            };

            subreddit_state.last_fetched = Some(get_epoch_ms()?);
            subreddit_state.pages_left -= 1;

            trace!("Subreddit has {} pages left", subreddit_state.pages_left);

            if subreddit_state.fetched_up_to.is_none() {
                trace!("Subreddit has no more pages left: {}", &subreddit);
                subreddit_state.pages_left = 0;
            }

            // If we've fetched all the pages, remove the subreddit from the list
            if subreddit_state.pages_left == 0 && !subreddit_state.always_fetch {
                subreddits.remove(&subreddit);
                let _: () = con.set(&subreddit, get_epoch_ms()?).await?;
                info!("Got custom subreddit: {:?}", &subreddit);
                let _: () = con.lrem("custom_subreddits_queue", 0, &subreddit).await?;
            } else if subreddit_state.pages_left == 0 {
                subreddit_state.pages_left = 10;
                subreddit_state.fetched_up_to = None;
            }
        }

        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::main]
async fn main() {
    initialise_observability!("reddit-downloader");

    println!("Initialised tracing!");

    let _guard = sentry::init((
        "https://75873f85a862465795299365b603fbb5@us.sentry.io/4504774760660992",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 0.01,
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
            capture_anyhow(&e);
            error!("Error: {:?}", e);
        }
    };
}
