use futures::{Future, StreamExt};
use mongodb::bson::{doc, Bson};
use mongodb::options::ClientOptions;
use redis::AsyncCommands;
use serde_derive::{Deserialize, Serialize};
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http};
use serenity::json::json;
use serenity::{all::EventHandler, async_trait};

use log::{debug, error, info, warn};
use tarpc::server::incoming::Incoming;
use tokio::sync::{Mutex, RwLock};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::env;
use std::io::Write;
use std::sync::Arc;
use anyhow::anyhow;


use futures::future::{self, Ready};
use tarpc::{
    client, context,
    server::{self, Channel},
    tokio_serde::formats::Bincode,
};

use post_subscriber::{Subscriber, Subscription, Bot};

#[derive(Clone)]
struct SubscriberServer {
    socket_addr: SocketAddr,
    db: Arc<Mutex<mongodb::Client>>,
    subscriptions: Arc<RwLock<Vec<Subscription>>>,
    discord_bb: Arc<Http>,
    discord_rs: Arc<Http>,
    redis: redis::aio::MultiplexedConnection,
    posthog: posthog::Client
}

impl Subscriber for SubscriberServer {
    async fn register_subscription(self, _: context::Context, subreddit: String, channel: u64, bot: Bot) -> Result<(), String> {
        info!("Registering subscription for subreddit {} to channel {}", subreddit, channel);

        let _ = self.posthog.capture("subscription_create", json!([("subreddit", &subreddit)]), &channel.to_string()).await;

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let subscription = Subscription {
            subreddit: subreddit,
            channel: channel,
            bot,
        };

        match coll.insert_one(&subscription, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string())
        };

        self.subscriptions.write().await.push(subscription);
        Ok(())
    }

    async fn delete_subscription(self, _: context::Context, subreddit: String, channel: u64, bot: Bot) -> Result<(), String> {
        info!("Deleting subscription for subreddit {} to channel {}", subreddit, channel);

        let _ = self.posthog.capture("subscription_delete", json!([("subreddit", &subreddit)]), &channel.to_string()).await;

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let filter = doc! {
            "subreddit": &subreddit,
            "channel": channel as i64,
            "bot": bot
        };

        match coll.delete_one(filter, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string())
        
        };
        self.subscriptions.write().await.retain(|sub| sub.subreddit != subreddit || sub.channel != channel);
        Ok(())
    }

    // List subscriptions for a channel
    async fn list_subscriptions(self, _: context::Context, channel: u64, bot: Bot) -> Result<Vec<Subscription>, String> {
        info!("Listing subscriptions");

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let filter = doc!{
            "channel": channel as i64,
            "bot": bot
        };

        let mut cursor = match coll.find(filter, None).await {
            Ok(cursor) => cursor,
            Err(e) => return Err(e.to_string())
        };
        let mut subscriptions = Vec::new();

        while let Some(sub) = cursor.next().await {
            match sub {
                Ok(sub) => {
                    subscriptions.push(sub);
                },
                Err(e) => return Err(e.to_string())
            }
        }

        Ok(subscriptions)
    }

    // Notify subscribers of a new post
    async fn notify(self, _: context::Context, subreddit: String, post_id: String) -> Result<(), String> {
        info!("Notifying subscribers of post {} in subreddit {}", post_id, subreddit);

        let subscriptions = self.subscriptions.read().await;
        debug!("Lock acquired, checking subscriptions {:?}", subscriptions);

        let filtered = subscriptions.iter().filter(|sub| sub.subreddit == subreddit).collect::<Vec<&Subscription>>();

        if filtered.len() == 0 {
            return Ok(());
        }

        let mut redis = self.redis.clone();

        debug!("Getting post {}", post_id);
        let mut post = match post_api::get_post_by_id(&format!("subreddit:{}:post:{}", subreddit, &post_id), None, &mut redis, None).await {
            Ok(post) => post,
            Err(e) => {
                warn!("Failed to get post: {:?}", e);
                return Err(e.to_string())
            }
        };
        debug!("Got post {:?}", post);

        // Remove the components because we don't want autopost and refresh options in this context
        post.components = None;

        let _ = self.posthog.capture("subreddit_new_post", json!([("subreddit", &subreddit)]), "").await;

        debug!("Filtered subscriptions {:?}", filtered);

        for sub in filtered {
            info!("Notifying channel {} of post {} in subreddit {}", sub.channel, post_id, subreddit);

            let _ = self.posthog.capture("notify_new_post", json!([("subreddit", &subreddit)]), &sub.channel.to_string()).await;

            let post = post.clone();

            let channel: ChannelId = sub.channel.into();
            
            // Turn post response into message
            let mut resp = CreateMessage::default();
            if let Some(em) = post.embed {
                resp = resp.embed(em);
            }

            if let Some(content) = post.content {
                resp = resp.content(content);
            }

            if let Some(attachment) = post.file {
                resp = resp.add_file(attachment);
            }

            match sub.bot {
                Bot::BB => {
                    match channel.send_message(&self.discord_bb, resp).await {
                        Ok(_) => (),
                        Err(e) => return Err(e.to_string())
                    }
                },
                Bot::RS => {
                    match channel.send_message(&self.discord_rs, resp).await {
                        Ok(_) => (),
                        Err(e) => return Err(e.to_string())
                    }
                }
            }
        }

        Ok(())
    }

    async fn watched_subreddits(self, _: context::Context) -> Result<HashSet<String>, String> {
        let subscriptions = self.subscriptions.read().await;
        let subreddits: HashSet<String> = subscriptions.iter().map(|sub| sub.subreddit.clone()).collect();

        info!("Listing watched subreddits {:?}", subreddits);

        Ok(subreddits)
    }
}


async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {}

#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    debug!("Starting...");

    let token = env::var("DISCORD_TOKEN_BB").expect("Expected DISCORD_TOKEN_BB in the environment");
    let intents = GatewayIntents::empty();
    let mut client_bb = serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");
    let http_bb = client_bb.http.clone();

    let token = env::var("DISCORD_TOKEN_RS").expect("Expected DISCORD_TOKEN_RS in the environment");
    let intents = GatewayIntents::empty();
    let mut client_rs = serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");
    let http_rs = client_rs.http.clone();

    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let mongo_url = env::var("MONGO_URL").expect("MONGO_URL not set");
    let mut client_options = ClientOptions::parse(mongo_url).await.unwrap();
    client_options.app_name = Some("Post Subscriber".to_string());
    let mongodb_client = Arc::new(Mutex::new(mongodb::Client::with_options(client_options).unwrap()));

    let existing_subs = {
        let client = mongodb_client.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let mut cursor = coll.find(None, None).await.expect("Failed to get subscriptions");
        let mut subscriptions = Vec::new();

        while let Some(sub) = cursor.next().await {
            match sub {
                Ok(sub) => {
                    subscriptions.push(sub);
                },
                Err(e) => {
                    error!("Failed to get subscription: {:?}", e);
                    break;
                }
            }
        }

        subscriptions
    };

    let subscriptions = Arc::new(RwLock::new(existing_subs));

    let redis_url = env::var("REDIS_URL").expect("REDIS_URL not set");
    let redis_client = redis::Client::open(redis_url).unwrap();
    let mut redis = redis_client.get_multiplexed_async_connection().await.expect("Can't connect to redis");

    let total_shards_bb: redis::RedisResult<u32> = redis.get("total_shards_booty-bot").await;
    let total_shards_bb: u32 = total_shards_bb.expect("Failed to get or convert total_shards");

    let total_shards_rs: redis::RedisResult<u32> = redis.get("total_shards_r-slash").await;
    let total_shards_rs: u32 = total_shards_rs.expect("Failed to get or convert total_shards");

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default).await.unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    let handle = tokio::spawn(listener
        // Ignore accept errors.
        .filter_map(|r| future::ready(r.ok()))
        .map(server::BaseChannel::with_defaults)
        // serve is generated by the service attribute. It takes as input any type implementing
        // the generated SubscriberServer trait.
        .map(move |channel| {
            let server = SubscriberServer {
                socket_addr: channel.transport().peer_addr().unwrap(),
                db: Arc::clone(&mongodb_client),
                subscriptions: Arc::clone(&subscriptions),
                discord_bb: Arc::clone(&http_bb),
                discord_rs: Arc::clone(&http_rs),
                redis: redis.clone(),
                posthog: posthog_client.clone()
            };
            channel.execute(server.serve()).for_each(spawn)
        })
        // Max 10 channels.
        .buffer_unordered(10)
        .for_each(|_| async {})
    );

    info!("Started tarpc server, connecting to discord...");

    tokio::spawn(async move {
        if let Err(why) = client_bb.start_shard(0, total_shards_bb).await {
            error!("Client error: {:?}", why);
        }
    });

    tokio::spawn(async move {
        if let Err(why) = client_rs.start_shard(0, total_shards_rs).await {
            error!("Client error: {:?}", why);
        }
    });
    
    handle.await.expect("Failed to run server");
    error!("Server stopped!");
}