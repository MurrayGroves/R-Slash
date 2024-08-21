use futures::{Future, StreamExt};
use mongodb::bson::{doc, Bson};
use mongodb::options::ClientOptions;
use redis::AsyncCommands;
use serde_derive::{Deserialize, Serialize};
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http};
use serenity::json::json;
use serenity::{all::EventHandler, async_trait};

use anyhow::anyhow;
use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tarpc::server::incoming::Incoming;
use tokio::sync::{Mutex, RwLock};

use futures::future::{self, Ready};
use tarpc::{
    client, context,
    server::{self, Channel},
    tokio_serde::formats::Bincode,
};

use post_subscriber::{Bot, Subscriber, Subscription};

struct Subscriptions {
    by_subreddit: HashMap<String, HashSet<Arc<Subscription>>>,
    by_channel: HashMap<u64, HashSet<Arc<Subscription>>>,
}

#[derive(Clone)]
struct SubscriberServer {
    socket_addr: SocketAddr,
    db: Arc<Mutex<mongodb::Client>>,
    subscriptions: Arc<RwLock<Subscriptions>>,
    discord_bb: Arc<Http>,
    discord_rs: Arc<Http>,
    redis: redis::aio::MultiplexedConnection,
    posthog: posthog::Client,
}

impl Subscriber for SubscriberServer {
    async fn register_subscription(
        self,
        _: context::Context,
        subreddit: String,
        channel: u64,
        bot: Bot,
    ) -> Result<(), String> {
        info!(
            "Registering subscription for subreddit {} to channel {}",
            subreddit, channel
        );

        // If this channel is already subscribed to this subreddit, return an error
        let subscriptions = self.subscriptions.read().await;

        let subscription = Subscription {
            subreddit: subreddit.clone(),
            channel,
            bot,
            added_at: chrono::Utc::now().timestamp(),
        };

        if let Some(subs) = subscriptions.by_subreddit.get(&subreddit) {
            if subs.contains(&subscription) {
                return Err("Already subscribed".to_string());
            }
        };
        drop(subscriptions);

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> =
            client.database("state").collection("subscriptions");

        match coll.insert_one(&subscription, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string()),
        };

        let mut subs = self.subscriptions.write().await;
        let sub = Arc::new(subscription);
        subs.by_subreddit
            .entry(sub.subreddit.clone())
            .or_insert(HashSet::new())
            .insert(sub.clone());

        subs.by_channel
            .entry(sub.channel)
            .or_insert(HashSet::new())
            .insert(sub);
        Ok(())
    }

    async fn delete_subscription(
        self,
        _: context::Context,
        subreddit: String,
        channel: u64,
        bot: Bot,
    ) -> Result<(), String> {
        info!(
            "Deleting subscription for subreddit {} to channel {}",
            subreddit, channel
        );

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> =
            client.database("state").collection("subscriptions");

        let filter = doc! {
            "subreddit": &subreddit,
            "channel": channel as i64,
            "bot": bot.clone(),
        };

        match coll.delete_one(filter, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string()),
        };
        let mut subs = self.subscriptions.write().await;

        subs.by_channel
            .get_mut(&channel)
            .ok_or(
                "Tried to remove subscription by channel that already didn't exist!".to_string(),
            )?
            .remove(&Subscription {
                subreddit: subreddit.clone(),
                channel,
                bot: bot.clone(),
                added_at: 0,
            });

        subs.by_subreddit
            .get_mut(&subreddit)
            .ok_or("Tried to remove subscription by sub that already didn't exist!".to_string())?
            .remove(&Subscription {
                subreddit: subreddit.clone(),
                channel,
                bot,
                added_at: 0,
            });
        Ok(())
    }

    // List subscriptions for a channel
    async fn list_subscriptions(
        self,
        _: context::Context,
        channel: u64,
        bot: Bot,
    ) -> Result<Vec<Subscription>, String> {
        info!("Listing subscriptions for channel {}", channel);

        let subscriptions = self.subscriptions.read().await;

        let filtered = match subscriptions.by_channel.get(&channel) {
            Some(x) => x,
            None => return Ok(vec![]),
        };

        let cloned = filtered
            .iter()
            .map(|sub| (**sub).clone())
            .filter(|x| x.bot == bot)
            .collect();

        Ok(cloned)
    }

    // Notify subscribers of a new post
    async fn notify(
        self,
        _: context::Context,
        subreddit: String,
        post_id: String,
    ) -> Result<(), String> {
        info!(
            "Notifying subscribers of post {} in subreddit {}",
            post_id, subreddit
        );

        let subscriptions = self.subscriptions.read().await;
        debug!("Lock acquired, checking subscriptions");

        let filtered = match subscriptions.by_subreddit.get(&subreddit) {
            Some(x) => x,
            None => return Ok(()),
        };

        if filtered.len() == 0 {
            return Ok(());
        }

        let mut redis = self.redis.clone();

        debug!("Getting post {}", post_id);
        let mut post = match post_api::get_post_by_id(
            &format!("subreddit:{}:post:{}", subreddit, &post_id),
            None,
            &mut redis,
            None,
        )
        .await
        {
            Ok(post) => post,
            Err(e) => {
                warn!("Failed to get post: {:?}", e);
                return Err(e.to_string());
            }
        };
        debug!("Got post {:?}", post);

        let timestamp: i64 = match redis
            .hget(
                format!("subreddit:{}:post:{}", subreddit, &post_id),
                "timestamp",
            )
            .await
        {
            Ok(ts) => ts,
            Err(e) => {
                warn!("Failed to get timestamp: {:?}", e);
                return Err(e.to_string());
            }
        };

        // Remove the components because we don't want autopost and refresh options in this context
        post.components = None;

        let _ = self
            .posthog
            .capture(
                "subreddit_new_post",
                json!([("subreddit", &subreddit)]),
                "post-subscriber",
            )
            .await;

        debug!("Filtered subscriptions {:?}", filtered);

        for sub in filtered {
            if sub.added_at > timestamp {
                info!("Skipping notification for post {} in subreddit {} to channel {} because it was added after the post", post_id, subreddit, sub.channel);
                continue;
            }

            info!(
                "Notifying channel {} of post {} in subreddit {}",
                sub.channel, post_id, subreddit
            );

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
                Bot::BB => match channel.send_message(&self.discord_bb, resp).await {
                    Ok(_) => (),
                    Err(e) => return Err(e.to_string()),
                },
                Bot::RS => match channel.send_message(&self.discord_rs, resp).await {
                    Ok(_) => (),
                    Err(e) => return Err(e.to_string()),
                },
            }
        }

        Ok(())
    }

    async fn watched_subreddits(self, _: context::Context) -> Result<HashSet<String>, String> {
        info!("Listing watched subreddits");
        let subscriptions = self.subscriptions.read().await;

        let subreddits: HashSet<String> = subscriptions
            .by_subreddit
            .keys()
            .map(|x| x.clone())
            .collect();

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
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();

    debug!("Starting...");

    let token = env::var("DISCORD_TOKEN_BB").expect("Expected DISCORD_TOKEN_BB in the environment");
    let intents = GatewayIntents::empty();
    let mut client_bb = serenity::Client::builder(&token, intents)
        .event_handler(Handler)
        .await
        .expect("Err creating client");
    let http_bb = client_bb.http.clone();

    let token = env::var("DISCORD_TOKEN_RS").expect("Expected DISCORD_TOKEN_RS in the environment");
    let intents = GatewayIntents::empty();
    let mut client_rs = serenity::Client::builder(&token, intents)
        .event_handler(Handler)
        .await
        .expect("Err creating client");
    let http_rs = client_rs.http.clone();

    let posthog_key: String = env::var("POSTHOG_API_KEY")
        .expect("POSTHOG_API_KEY not set")
        .parse()
        .expect("Failed to convert POSTHOG_API_KEY to string");
    let posthog_client =
        posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let mongo_url = env::var("MONGO_URL").expect("MONGO_URL not set");
    let mut client_options = ClientOptions::parse(mongo_url).await.unwrap();
    client_options.app_name = Some("Post Subscriber".to_string());
    let mongodb_client = Arc::new(Mutex::new(
        mongodb::Client::with_options(client_options).unwrap(),
    ));

    let existing_subs = {
        let client = mongodb_client.lock().await;
        let coll: mongodb::Collection<Subscription> =
            client.database("state").collection("subscriptions");

        let mut cursor = coll
            .find(None, None)
            .await
            .expect("Failed to get subscriptions");
        let mut subscriptions = Vec::new();

        while let Some(sub) = cursor.next().await {
            match sub {
                Ok(sub) => {
                    subscriptions.push(sub);
                }
                Err(e) => {
                    error!("Failed to get subscription: {:?}", e);
                    break;
                }
            }
        }

        subscriptions
    };

    let mut by_subreddit = HashMap::new();
    let mut by_channel = HashMap::new();

    for sub in existing_subs {
        let sub = Arc::new(sub);
        by_subreddit
            .entry(sub.subreddit.clone())
            .or_insert_with(HashSet::new)
            .insert(sub.clone());
        by_channel
            .entry(sub.channel.clone())
            .or_insert_with(HashSet::new)
            .insert(sub);
    }

    let subscriptions = Arc::new(RwLock::new(Subscriptions {
        by_subreddit,
        by_channel,
    }));

    let redis_url = env::var("REDIS_URL").expect("REDIS_URL not set");
    let redis_client = redis::Client::open(redis_url).unwrap();
    let mut redis = redis_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis");

    let total_shards_bb: redis::RedisResult<u32> = redis.get("total_shards_booty-bot").await;
    let total_shards_bb: u32 = total_shards_bb.expect("Failed to get or convert total_shards");

    let total_shards_rs: redis::RedisResult<u32> = redis.get("total_shards_r-slash").await;
    let total_shards_rs: u32 = total_shards_rs.expect("Failed to get or convert total_shards");

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    let handle = tokio::spawn(
        listener
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
                    posthog: posthog_client.clone(),
                };
                channel.execute(server.serve()).for_each({
                    debug!("received connection");
                    spawn
                })
            })
            // Max 10 channels.
            .buffer_unordered(99999999)
            .for_each(|_| async {}),
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
