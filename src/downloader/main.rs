use log::*;
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use tokio::time::{sleep, Duration};
use futures::{lock::Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;

use redis::Commands;

use reqwest::header::{USER_AGENT, HeaderMap};
use serenity::prelude::TypeMapKey;
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

impl TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
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
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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
async fn get_subreddit(subreddit: String, con: &mut redis::Connection, web_client: &reqwest::Client, reddit_client: String, reddit_secret: String, device_id: Option<String>, gfycat_token: &mut oauthToken, imgur_client: String) { // Get the top 1000 most recent posts and store them in the DB
    debug!("Placeholder: {:?}", subreddit);

    let access_token = get_access_token(con, reddit_client.clone(), reddit_secret.clone(), web_client, device_id).await;

    let mut keys = Vec::new();
    let url_base = format!("https://reddit.com/r/{}/hot.json?limit=100", subreddit);
    let mut url = String::new();
    let mut after = String::new();
    let mut existing_posts: Vec<String> = redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(-1i64).query(con).unwrap();
    debug!("Existing posts: {:?}", existing_posts);
    for x in 0..10 {
        debug!("Making {}th request to reddit", x);

        if url.len() == 0 {
            url = url_base.clone();
        }

        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, "Discord:RSlash:v1.0.0 (by /u/murrax2)".parse().unwrap());
        headers.insert("Authorization", format!("bearer {}", access_token.clone()).parse().unwrap());

        debug!("{}", url);
        let res = web_client
            .get(&url)
            .headers(headers)
            .send()
            .await.expect("Failed to request from reddit");

        debug!("Finished request");
        let last_requested = get_epoch_ms();
        debug!("Got time");
        let text = match res.text().await {
            Ok(x) => x,
            Err(_) => {
                error!("Failed to get text from reddit");
                return;
            }
        };
        debug!("Matched text");
        let results: serde_json::Value = match serde_json::from_str(&text) {
            Ok(x) => x,
            Err(_) => {
                error!("Failed to parse JSON from Reddit");
                return;
            }
        };

        let results = results.get("data").unwrap().get("children").unwrap().as_array().unwrap();

        if results.len() == 0 {
            debug!("No results");
            break;
        }

        after = results[results.len() -1 ]["data"]["id"].as_str().unwrap().to_string();
        url = format!("{}&after=t3_{}", url_base, after);

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

                    if gfycat_token.expires_at < get_epoch_ms() - 1000 {
                        gfycat_token.refresh().await;
                    }

                    let auth = format!("Bearer {}", gfycat_token.token);

                    let res = match web_client
                        .get(format!("https://api.gfycat.com/v1/gfycats/{}", id))
                        .header("Authorization", auth)
                        .send()
                        .await {
                            Ok(x) => x,
                            Err(x) => {
                                error!("Failed to request from gfycat: {:?}", x);
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
                                warn!("Imgur request failed: {}", y);
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
            //post_scores.push(post["score"].clone().as_i64().unwrap_or(0));
            //debug!("Post Scores {:?}", post_scores);

            let timestamp = post["created_utc"].as_f64().unwrap() as u64;

            let post_object = Post {
                score: post["score"].as_i64().unwrap_or(0),
                url: format!("https://reddit.com{}", post["permalink"].to_string().replace('"', "")),
                title: post["title"].to_string().replace('"', ""),
                embed_url: url.to_string().replace('"', ""),
                author: post["author"].to_string().replace('"', ""),
                id: post["id"].to_string().replace('"', ""),
                timestamp: timestamp,
            };

            posts.push(post_object);
        }

        let mut new_post_count = 0;
        for post in posts.clone() {
            let key = format!("subreddit:{}:post:{}", subreddit.clone(), post.id.replace('"', ""));
            keys.push(key.clone());
            let value = Vec::from(post);
            let _:() = con.hset_multiple(key, &value).unwrap();
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
            let _:() = con.del(format!("subreddit:{}:posts", subreddit)).unwrap();
            let _:() = con.lpush(format!("subreddit:{}:posts", subreddit), keys.clone()).unwrap();
        }

        let time_since_request = get_epoch_ms() - last_requested;
        if time_since_request < 2000 {
            debug!("Waiting {:?}ms", 2000 - time_since_request);
            sleep(Duration::from_millis(2000 - time_since_request)).await; // Reddit rate limit is 30 requests per minute, so must wait at least 2 seconds between requests
        }
    }
}

#[derive(Debug)]
struct oauthToken {
    token: String,
    expires_at: u64,
    client_id: String,
    client_secret: String,
    url: String,
}

impl oauthToken {
    async fn new(client_id: String, client_secret: String, url: String) -> Result<oauthToken, Box<dyn std::error::Error>> {
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

        let results: serde_json::Value = res.json().await.unwrap();

        Ok(oauthToken {
            token: results["access_token"].to_string(),
            expires_at: results["expires_in"].as_u64().unwrap() + get_epoch_ms(),
            client_id,
            client_secret,
            url,
        })
    }

    async fn refresh(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let new = oauthToken::new(self.client_id.clone(), self.client_secret.clone(), self.url.clone()).await?;
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
async fn download_loop(data: Arc<Mutex<HashMap<String, ConfigValue>>>) {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_connection().expect("Can't connect to redis");

    let web_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build().expect("Failed to build client");

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

    let gfycat_client = env::var("GFYCAT_CLIENT").expect("GFYCAT_CLIENT not set");
    let gfycat_secret = env::var("GFYCAT_SECRET").expect("GFYCAT_SECRET not set");
    let imgur_client = env::var("IMGUR_CLIENT").expect("IMGUR_CLIENT not set");
    let do_custom = env::var("DO_CUSTOM").expect("DO_CUSTOM not set");
    let mut gfycat_token = oauthToken::new(gfycat_client, gfycat_secret, "https://api.gfycat.com/v1/oauth/token".to_string()).await.expect("Failed to get gfycat token");

    let mut client_options = mongodb::options::ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some("Downloader".to_string());

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

    if do_custom == "true".to_string() {
        loop {
            sleep(Duration::from_millis(100)).await;
            let custom_subreddits: Vec<String> = redis::cmd("LRANGE").arg("custom_subreddits_queue").arg(0i64).arg(0i64).query(&mut con).unwrap();
            if custom_subreddits.len() > 0 {
                let custom = custom_subreddits[0].clone();
                get_subreddit(custom.clone().to_string(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None, &mut gfycat_token, imgur_client.clone()).await;
                let _:() = con.set(&custom, get_epoch_ms()).unwrap();
                info!("Got custom subreddit: {:?}", custom);
                let _:() = con.lrem("custom_subreddits_queue", 0, custom).unwrap();
            }
        }
    }

    let mut last_run = 0;
    loop {
        if last_run > (get_epoch_ms() - 10000*60) { // Only download every 10 minutes, to avoid rate limiting (and also it's just not necessary)
            sleep(Duration::from_millis(100)).await;
            continue;
        }
        last_run = get_epoch_ms();

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
            let last_updated = con.get(&subreddit).unwrap_or(0u64);
            debug!("{:?} was last updated at {:?}", &subreddit, last_updated);

            let custom_subreddits: Vec<String> = redis::cmd("LRANGE").arg("custom_subreddits_queue").arg(0i64).arg(0i64).query(&mut con).unwrap();
            if custom_subreddits.len() > 0 {
                let custom = custom_subreddits[0].clone();
                get_subreddit(custom.clone().to_string(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None, &mut gfycat_token, imgur_client.clone()).await;
                let _:() = con.set(&custom, get_epoch_ms()).unwrap();
                info!("Got custom subreddit: {:?}", custom);
                let _:() = con.lrem("custom_subreddits_queue", 0, custom).unwrap();
            }
            get_subreddit(subreddit.clone().to_string(), &mut con, &web_client, reddit_client.clone(), reddit_secret.clone(), None, &mut gfycat_token, imgur_client.clone()).await;
            let _:() = con.set(&subreddit, get_epoch_ms()).unwrap();
            info!("Got subreddit: {:?}", subreddit);
        }

        info!("All subreddits updated");

    }
}


#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: Downloader: {}", record.level(), record.args())
    })
    .init();

    let reddit_secret = env::var("REDDIT_TOKEN").expect("REDDIT_TOKEN not set");
    let reddit_client = env::var("REDDIT_CLIENT_ID").expect("REDDIT_CLIENT_ID not set");

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("reddit_secret".to_string(), ConfigValue::String(reddit_secret.to_string())),
        ("reddit_client".to_string(), ConfigValue::String(reddit_client.to_string())),
    ]);


    let mut data = Arc::new(Mutex::new(contents));

    info!("Starting loops");
    download_loop(data.clone()).await;
}