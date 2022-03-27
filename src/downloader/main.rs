use log::*;
use std::{fs, thread};
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use crossbeam_utils;
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::hash::{Hash, Hasher};
use tokio_tungstenite;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use futures::prelude::stream::{SplitSink, SplitStream};
use tungstenite::Message;
use futures::{TryFutureExt, SinkExt, StreamExt, lock::Mutex};
use std::fmt::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::{RwLockWriteGuard, Arc};
use futures::executor::block_on;
use std::fs::File;
use serenity::http::Http;
use serde_json::Value;
use redis::Client;
use redis::Commands;
use rand::distributions::WeightedIndex;
use rand::thread_rng;
use rand::prelude::Distribution;
use reqwest::header::{USER_AGENT, HeaderMap};
use serenity::model::guild::Region::UsEast;
use serenity::prelude::TypeMapKey;

/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    Bool(bool),
    String(String),
    websocket_write(SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>),
    websocket_read(SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>)
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
}

impl TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
}

/// Represents a Reddit post
#[derive(Debug)]
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
}

impl From<Post> for Vec<(String, String)> {
    fn from(post: Post) -> Vec<(String, String)> {
        vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url.to_string()),
            ("title".to_string(), post.title.to_string()),
            ("embed_url".to_string(), post.embed_url.to_string()),
            ("author".to_string(), post.author.to_string()),
            ("id".to_string(), post.id.to_string())
        ]
    }
}

struct Handler;

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Makes a websocket request to the [coordinator](coordinator)
///
/// Returns a String containing the coordinator's response.
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
/// ## request
/// A [HashMap](std::collections::HashMap) of [String](String) - [Value](serde_json::Value) mappings.
async fn websocket_request(data: &mut Arc<Mutex<HashMap<String, ConfigValue>>>, request: HashMap<&str, serde_json::Value>) -> String {
    let mut data = data.lock().await;
    let request = serde_json::to_string(&request).unwrap();

    let mut websocket_write = match data.get_mut("coordinator_write").unwrap() {
        ConfigValue::websocket_write(x) => Ok(x),
        _ => Err(0)
    }.unwrap();

    websocket_write.send(tungstenite::Message::from(request.as_bytes())).await;

    let mut websocket_read = match data.get_mut("coordinator_read").unwrap() {
        ConfigValue::websocket_read(x) => Ok(x),
        _ => Err(0)
    }.unwrap();


    let resp = websocket_read.next().await;
    let resp = resp.unwrap().unwrap().into_text().unwrap();

    return resp;
}

/// Send a heartbeat to the coordinator
///
/// Returns the current time since the Epoch
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn send_heartbeat(data: &mut Arc<Mutex<HashMap<String, ConfigValue>>>) -> u64 {
    let heartbeat = HashMap::from([("type", Value::from("heartbeat")), ("shard_id", Value::from(-1))]);
    websocket_request(data, heartbeat).await;
    return get_epoch_ms();
}

/// Starts the loop that sends heartbeats
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
/// ## initial_heartbeat
/// The time at which the initial heartbeat was made on startup
async fn heartbeat_loop(mut data: Arc<Mutex<HashMap<String, ConfigValue>>>, initial_heartbeat: u64) {
    debug!("heartbeat_loop started");
    let mut last_heartbeat = initial_heartbeat as isize;
    loop {
        if last_heartbeat == 0 {
            warn!("NO HEARTBEAT");
            sleep(Duration::from_millis(10000)).await; // Sleep until on_ready() has completed and first heartbeat is sent.
            continue;
        }
        let mut time_to_heartbeat:isize = (last_heartbeat + 57000) - (get_epoch_ms() as isize);  // How long until we need to send a heartbeat
        if time_to_heartbeat < 1500 {  // If less than 1.5 seconds until need to send a heartbeat
            debug!("SENDING HEARTBEAT");
            last_heartbeat = send_heartbeat(&mut data).await as isize;
            debug!("Heartbeat sent");
            time_to_heartbeat = 56000;
        }
        sleep(Duration::from_millis(time_to_heartbeat as u64)).await; // Sleep until we need to send a heartbeat
    }
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
async fn request_access_token(reddit_client: String, reddit_secret: String, web_client: &reqwest::Client, device_id: Option<String>) -> (String, u64) {
    let post_data = match device_id {
        Some(x) => format!("grant_type=https://oauth.reddit.com/grants/installed_client&\\device_id={}", x),
        None => "grant_type=client_credentials".to_string(),
    };

    let res = web_client
        .post("https://www.reddit.com/api/v1/access_token")
        .body(post_data)
        .basic_auth(reddit_client, Some(reddit_secret))
        .send()
        .await.expect("Failed to request access token from Reddit");

    let results: serde_json::Value = res.json().await.unwrap();
    let token = results.get("access_token").expect("Reddit did not return access token").to_string();
    let expires_in:u64 = results.get("expires_in").expect("Reddit did not provide expires_in").as_u64().unwrap();
    let expires_at = get_epoch_ms() + expires_in;

    return (token, expires_at);
}

/// Returns a String of the Reddit access token to use
///
/// # Arguments
/// ## con
/// The [connection](redis::Connection) to the redis DB.
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## device_id
/// If specified, will request an [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
async fn get_access_token(con: &mut redis::Connection, reddit_client: String, reddit_secret: String, web_client: &reqwest::Client, device_id: Option<String>) -> String { // Return a reddit access token
    let token_name = match device_id.clone() { // The key to grab from the DB
        Some(x) => x,
        _ => "default".to_string(),
    };


    let token = con.hget("reddit_tokens", token_name.clone());
    let token = match token {
        Ok(x) => x,
        Err(_) => {
            let token_results = request_access_token(reddit_client.clone(), reddit_secret.clone(), web_client, device_id.clone()).await;
            let access_token = token_results.0;
            let expires_at = token_results.1;

            let _:() = con.hset("reddit_tokens", token_name.clone(), format!("{},{}", access_token, expires_at)).expect("Failed to execute hset");

            format!("{},{}", access_token, expires_at)
        }

    };

    let expires_at:u64 = token.split(",").collect::<Vec<&str>>()[1].parse().unwrap();
    let mut access_token:String = token.split(",").collect::<Vec<&str>>()[0].parse().unwrap();

    if expires_at < get_epoch_ms() {
        let token_results = request_access_token(reddit_client.clone(), reddit_secret.clone(), web_client, device_id.clone()).await;
        access_token = token_results.0;
        let expires_at = token_results.1;

        let _:() = con.hset("reddit_tokens", token_name, format!("{},{}", access_token, expires_at)).expect("Failed to executed hset");
    }

    debug!("Reddit Token: {}", access_token.replace("\"", ""));
    return access_token;
}

/// Get the top 1000 most recent media posts and store them in the DB
///
/// # Arguments
/// ## subreddit
/// A [String](String) representing the subreddit name, without the `r/`
/// ## con
/// A [Connection](redis::Connection) to the Redis DB
/// ## web_client
/// The Reqwest [Client](reqwest::Client)
/// ## reddit_client
/// The Reddit client as [String](String)
/// ## reddit_secret
/// The Reddit access token as a [String](String)
/// ## device_id
/// None if a default subreddit, otherwise is the user's ID.
async fn get_subreddit(subreddit: String, con: &mut redis::Connection, web_client: &reqwest::Client, reddit_client: String, reddit_secret: String, device_id: Option<String>) { // Get the top 1000 most recent posts and store them in the DB
    debug!("Placeholder: {:?}", subreddit);

    let access_token = get_access_token(con, reddit_client.clone(), reddit_secret.clone(), web_client, device_id).await;

    let mut posts = Vec::new();

    let url_base = format!("https://api.pushshift.io/reddit/search/submission/?size=100&subreddit={}&domain=imgur.com,i.redd.it,redgifs.com,gfycat.com&user_removed=false&mod_removed=false", subreddit);
    let mut url = String::new();
    let mut real_scores:HashMap<String, i64> = HashMap::new();
    for x in 0..10 {
        let mut listing_ids = String::new();

        debug!("Making {}th request to pushshift", x);

        if url.len() == 0 {
            url = url_base.clone();
        }

        let res = web_client
            .get(&url)
            .send()
            .await.expect("Failed to request from pushshift");

        let results: serde_json::Value = res.json().await.unwrap();
        let results = results.get("data").unwrap().as_array().unwrap();

        let before = results[results.len() - 1]["created_utc"].clone();
        url = format!("{}&before={}", url_base, before);
        debug!("{}", url);

        let mut batch_size = 0;
        for post in results.clone() {
            let id = post["id"].as_str().expect("Failed to convert ID to str");
            listing_ids = format!("t3_{},{}", id, listing_ids);
            batch_size += 1;
            if batch_size >= 25 {
                let score_url = format!("https://reddit.com/by_id/{}.json", listing_ids);
                debug!("{}", score_url);

                let mut headers = HeaderMap::new();
                headers.insert(USER_AGENT, "Discord:RSlash:v1.0.0 (by /u/murrax2)".parse().unwrap());
                headers.insert("Authorization", format!("bearer {}", access_token.clone()).parse().unwrap());

                let res = web_client
                    .get(&score_url)
                    .headers(headers)
                    .send()
                    .await.expect("Failed to request from reddit api");

                let full_resp = res.text().await.unwrap();
                let results: serde_json::Value = full_resp.parse().unwrap();
                //debug!("Results: {:?}", results);
                let results = results.get("data").unwrap();
                let results = results.get("children").unwrap().as_array().unwrap();
                {
                    for post in results {
                        let post = &post["data"];
                        //debug!("{:?}", post["id"]);
                        //debug!("{:?}", post);
                        real_scores.insert(post["id"].as_str().unwrap().to_string(), post["score"].as_i64().unwrap_or(0));
                    }
                }

                posts = [posts, results.clone()].concat();
                batch_size = 0;
                listing_ids = String::new();
                sleep(Duration::from_millis(2000)).await; // Reddit rate limit is 30 requests per minute
            }
        }
    }

    let results = posts.clone();

    let mut posts = Vec::new();
    let mut post_scores = Vec::new();

    for post in results {
        // For gfycat get media/oembed/thumbnail_url, replace thumbs with giant, and remove size-restricted, and replace .gif with .mp4
        // If removed_by_category is present then post is removed
        // For imgur, get url, and replace extension with .mp4 (might not have any extension), if url has gallery in it, get url from media/oembed/thumbnail_url and replace with .mp4 again
        // For i.redd.it just take the L and use the url
        debug!("{:?} - {:?}", post["title"], post["url"]);
        let mut url = post["url"].clone().to_string();

        if post.get("removed_by_category").is_some() {
            continue;
        }

        if url.contains("gfycat") || url.contains("redgifs") {
            url = post["media"]["oembed"]["thumbnail_url"].clone().to_string();
            url = url.replace("thumbs", "giant");
            url = url.replace("size-restricted", "");
            url = url.replace(".gifv", ".mp4");
            url = url.replace(".gif", ".mp4");

        } else if url.contains("imgur") {
            if url.contains("gallery") {
                url = post["media"]["oembed"]["thumbnail_url"].clone().to_string();
                url = url.replace(".gifv", ".mp4");
                url = url.replace(".gif", ".mp4");
            } else {
                url = url.replace(".gifv", ".mp4");
                url = url.replace(".gif", ".mp4");
            }
        }

        let post = &post["data"];
        debug!("Post Score: {:?}", post["score"]);
        post_scores.push(post["score"].clone().as_i64().unwrap_or(0));
        debug!("Post Scores {:?}", post_scores);

        let post_object = Post {
            score: real_scores[post["id"].as_str().expect("Failed to convert post ID to str")],
            url: post["full_link"].to_string(),
            title: post["title"].to_string(),
            embed_url: url.to_string(),
            author: post["author"].to_string(),
            id: post["id"].to_string(),
        };

        posts.push(post_object);
    }

    debug!("Length of posts: {}", posts.len());

    // Normalise post_scores to all be bigger than 0
    let minimum = post_scores.iter().min().unwrap();

    let mut offset = 0;
    if minimum > &(0 as i64) {
        offset = 0;
    } else {
        offset = 1 - minimum;
    }

    post_scores.iter_mut().for_each(|x| *x += offset); // Add offset to each post score

    // This does a weighted shuffle on the posts
    let mut shuffled_posts = Vec::new();
    let mut rng = thread_rng();

    while posts.len() > 0 {
        let shuffler = WeightedIndex::new(&post_scores).expect("Failed to created WeightedIndex from post_scores");
        let post_index = shuffler.sample(&mut rng);
        shuffled_posts.push(posts.remove(post_index));
        post_scores.remove(post_index);
    }

    debug!("Shuffled Posts: {:?}", shuffled_posts);
    debug!("Length of shuffled posts: {:?}", shuffled_posts.len());

    let mut keys = Vec::new();
    for post in shuffled_posts {
        let key = format!("subreddit:{}:post:{}", subreddit.clone(), post.id);
        keys.push(key.clone());
        let value = Vec::from(post);
        let _:() = con.hset_multiple(key, &value).unwrap();
    }

    let _:() = con.del(format!("subreddit:{}:posts", subreddit)).unwrap();
    let _:() = con.lpush(format!("subreddit:{}:posts", subreddit), keys).unwrap();
    
}

/// Start the downloading loop
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn download_loop(data: Arc<Mutex<HashMap<String, ConfigValue>>>) {
    let db_client = redis::Client::open("redis://127.0.0.1/").unwrap();
    let mut con = db_client.get_connection().expect("Can't connect to redis");

    let web_client = reqwest::Client::new();

    let mut reddit_secret = String::new();
    let mut reddit_client = String::new();
    {
        let mut data_lock = data.lock().await;
        reddit_secret = match data_lock.get("reddit_secret").unwrap() {
            ConfigValue::String(x) => x,
            _ => panic!("Failed to get reddit_secret")
        }.clone();

        reddit_client = match data_lock.get("reddit_client").unwrap() {
            ConfigValue::String(x) => x,
            _ => panic!("Failed to get reddit_client")
        }.clone();
    }

    let mut last_run = 0;
    loop {
        if last_run > (get_epoch_ms() - 10000*60) { // Only download every 10 minutes, to avoid rate limiting (and also it's just not necessary)
            sleep(Duration::from_millis((last_run + 10000*60) - get_epoch_ms())).await;
            continue;
        }
        last_run = get_epoch_ms();

        let get_subreddit_list = HashMap::from([("type", Value::from("get_subreddit_list"))]);
        let resp = websocket_request(&mut data.clone(), get_subreddit_list).await;

        debug!("Response to get_subreddit_list received: {:?}", resp);

        let subreddits:Vec<String> = serde_json::from_str(&resp).unwrap();

        for subreddit in subreddits {
            debug!("Getting subreddit: {:?}", subreddit);
            let last_updated = con.get(&subreddit).unwrap_or(0u64);
            debug!("{:?} was last updated at {:?}", &subreddit, last_updated);
            get_subreddit(subreddit.clone(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None).await;
            let _:() = con.set(&subreddit, get_epoch_ms()).unwrap();
            debug!("Got subreddit: {:?}", subreddit);
        }

        debug!("All subreddits updated");

    }
}

/// Run on startup
///
/// Reads config file, starts websocket server, gets shard info from Discord, starts loops and starts sub-programs.
#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: Downloader: {}", record.level(), record.args())
    })
    .init();

    let coordinator = tokio_tungstenite::connect_async("ws://127.0.0.1:9002").await.expect("Failed to connect to coordinator");
    let response = coordinator.1.into_body();
    let coordinator = coordinator.0;
    let mut streams = coordinator.split();
    let mut write = streams.0;
    let read = streams.1;

    let mut write = ConfigValue::websocket_write(write);

    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
    .expect("config.json is not proper JSON");

    let reddit_secret = config.get("reddit_secret").unwrap().as_str().unwrap();
    let reddit_client = config.get("reddit_client").unwrap().as_str().unwrap();

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("coordinator_write".to_string(), write),
        ("coordinator_read".to_string(), ConfigValue::websocket_read(read)),
        ("last_heartbeat".to_string(), ConfigValue::U64(0)),
        ("reddit_secret".to_string(), ConfigValue::String(reddit_secret.to_string())),
        ("reddit_client".to_string(), ConfigValue::String(reddit_client.to_string())),
    ]);


    let mut data = Arc::new(Mutex::new(contents));

    let initial_heartbeat = send_heartbeat(&mut data).await;

    debug!("Starting loops");
    tokio::spawn(download_loop(data.clone()));
    block_on(heartbeat_loop(data, initial_heartbeat));
}