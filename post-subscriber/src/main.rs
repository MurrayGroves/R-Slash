use futures::{Future, StreamExt};
use mongodb::bson::doc;
use mongodb::options::ClientOptions;
use redis::AsyncTypedCommands;
use serde_json::json;
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, GuildId, Http, Token};
use serenity::{all::EventHandler, async_trait};

use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Instant, interval, timeout};
use user_config_manager::{TextAllowLevel, get_channel_config};

use futures::future;
use metrics::{counter, gauge, histogram};
use post_subscriber::{Bot, Subscriber, Subscription};
use rslash_common::{get_post_content_type, initialise_observability, span_filter};
use tarpc::{
    server::{self, Channel},
    tokio_serde::formats::Bincode,
};

use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{logs, trace};
use tarpc::context::Context;
use tracing_subscriber::{
    EnvFilter, Layer, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
};

struct Subscriptions {
    by_subreddit: HashMap<String, HashSet<Arc<Subscription>>>,
    by_channel: HashMap<u64, HashSet<Arc<Subscription>>>,
}

struct PostAlert<'a> {
    message: CreateMessage<'a>,
    subscriptions: Vec<Arc<Subscription>>,
    timestamp: Instant,
    text_level: TextAllowLevel,
}

#[derive(Clone)]
struct SubscriberServer<'a> {
    db: mongodb::Client,
    subscriptions: Arc<RwLock<Subscriptions>>,
    discord_bb: Arc<Http>,
    discord_rs: Arc<Http>,
    redis: redis::aio::MultiplexedConnection,
    queued_alerts: Arc<Mutex<VecDeque<PostAlert<'a>>>>,
}

impl SubscriberServer<'_> {
    #[tracing::instrument(skip(self))]
    async fn watch_alerts(self) {
        let mut outer_interval = interval(tokio::time::Duration::from_millis(100));
        let inner_duration = tokio::time::Duration::from_millis(1000 / 35); // Send alerts at max of 35 per second (Discord global rate limit is 50 reqs per second)
        loop {
            outer_interval.tick().await;
            let mut queued_alerts = self.queued_alerts.lock().await;
            let next_alert = match queued_alerts.pop_front() {
                Some(x) => x,
                None => continue,
            };
            drop(queued_alerts);

            counter!("subscriber_processed_alert_batches").increment(1);

            let mut last_alert = Instant::now() - inner_duration;
            for alert in next_alert.subscriptions {
                // Ensure we don't send more than 35 messages per second
                if last_alert.elapsed() < inner_duration {
                    debug!(
                        "Waiting to send next alert, last alert was sent {} seconds ago",
                        last_alert.elapsed().as_secs()
                    );
                    debug!("Waiting for {} seconds", inner_duration.as_secs_f64());
                    tokio::time::sleep(inner_duration - last_alert.elapsed()).await;
                }
                last_alert = Instant::now();
                let channel: ChannelId = alert.channel.into();
                let http = match alert.bot {
                    Bot::BB => &self.discord_bb,
                    Bot::RS => &self.discord_rs,
                };
                debug!(
                    "Sending alert for {} to channel {}",
                    alert.subreddit, channel
                );

                let text_allow_level = match get_channel_config(&mut self.db.clone(), channel).await
                {
                    Ok(x) => x.text_allowed.unwrap_or_default(),
                    Err(e) => {
                        info!("Error getting channel config: {:?}", e);
                        error!("Error getting channel config");
                        continue;
                    }
                };

                if !text_allow_level.allows_for(next_alert.text_level) {
                    debug!(
                        "Post with {:?} doesn't match text allow level of {:?}",
                        next_alert.text_level, text_allow_level
                    );
                    continue;
                }

                let response = match timeout(
                    Duration::from_secs(30),
                    channel
                        .widen()
                        .send_message(http, next_alert.message.clone()),
                )
                .await
                {
                    Ok(resp) => resp,
                    Err(_) => {
                        error!("Timeout while sending message to channel");
                        continue;
                    }
                };

                match response {
                    Ok(_) => {
                        let elapsed = next_alert.timestamp.elapsed();
                        debug!(
                            "Sent {} to channel {}, was added {} seconds ago",
                            alert.subreddit,
                            channel,
                            elapsed.as_secs()
                        );
                        histogram!("subscriber_notification_latency")
                            .record(elapsed.as_millis() as f64);
                        counter!("subscriber_sent_messages").increment(1);
                    }
                    Err(e) => {
                        warn!("Failed to send message to channel {}: {:?}", channel, e);
                        if let serenity::Error::Http(e) = e {
                            if let serenity::http::HttpError::UnsuccessfulRequest(e) = e {
                                if e.error.code.0 == 10003 {
                                    debug!("Channel doesn't exist anymore, deleting subscription");
                                    let mut subscriptions = self.subscriptions.write().await;
                                    match subscriptions.by_channel.get_mut(&alert.channel) {
                                        Some(x) => {
                                            x.remove(&*alert);
                                        }
                                        None => {
                                            warn!(
                                                "Tried to delete subscription by channel that already didn't exist!"
                                            );
                                        }
                                    };
                                    match subscriptions.by_subreddit.get_mut(&alert.subreddit) {
                                        Some(x) => {
                                            x.remove(&*alert);
                                        }
                                        None => {
                                            warn!(
                                                "Tried to delete subscription by sub that already didn't exist!"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Subscriber for SubscriberServer<'_> {
    #[tracing::instrument(skip(self))]
    async fn register_subscription(
        self,
        _: Context,
        subreddit: String,
        channel: u64,
        bot: Bot,
    ) -> Result<(), String> {
        info!(
            "Registering subscription for subreddit {} to channel {}",
            subreddit, channel
        );

        gauge!("subscriber_registered_subscriptions").increment(1);

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

        let coll: mongodb::Collection<Subscription> =
            self.db.database("state").collection("subscriptions");

        match coll.insert_one(&subscription).await {
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

    #[tracing::instrument(skip(self))]
    async fn delete_subscription(
        self,
        _: Context,
        subreddit: String,
        channel: u64,
        bot: Bot,
    ) -> Result<(), String> {
        info!(
            "Deleting subscription for subreddit {} to channel {}",
            subreddit, channel
        );

        gauge!("subscriber_registered_subscriptions").decrement(1);

        let coll: mongodb::Collection<Subscription> =
            self.db.database("state").collection("subscriptions");

        let filter = doc! {
            "subreddit": &subreddit,
            "channel": channel as i64,
            "bot": bot.clone(),
        };

        match coll.delete_one(filter).await {
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
    #[tracing::instrument(skip(self))]
    async fn list_subscriptions(
        self,
        _: Context,
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
    #[tracing::instrument(skip(self))]
    async fn notify(self, _: Context, subreddit: String, post_id: String) -> Result<(), String> {
        info!(
            "Notifying subscribers of post {} in subreddit {}",
            post_id, subreddit
        );

        counter!("subscriber_posts_received").increment(1);

        let subscriptions = self.subscriptions.read().await;
        debug!("Lock acquired, checking subscriptions");

        let filtered = match subscriptions.by_subreddit.get(&subreddit.to_lowercase()) {
            Some(x) => x,
            None => return Ok(()),
        };

        if filtered.len() == 0 {
            return Ok(());
        }

        let mut redis = self.redis.clone();

        debug!("Getting post {}", post_id);
        let post = match post_api::get_post_by_id(
            &format!("subreddit:{}:post:{}", subreddit, &post_id),
            None,
            &mut redis,
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
        debug!("Filtered subscriptions {:?}", filtered);

        let text_allow_level = match get_post_content_type(
            &mut self.redis.clone(),
            &format!("subreddit:{}:post:{}", subreddit.to_lowercase(), &post_id),
        )
        .await
        {
            Ok(x) => x,
            Err(e) => {
                error!("Failed to get post text allow level");
                return Err("Failed to get post text allow level".to_string());
            }
        };

        let alert = PostAlert {
            message: post.buttonless_message(),
            subscriptions: filtered.into_iter().map(|x| (*x).clone()).collect(),
            timestamp: Instant::now(),
            text_level: text_allow_level,
        };

        let mut queued_alerts = self.queued_alerts.lock().await;
        queued_alerts.push_back(alert);

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    async fn watched_subreddits(self, _: Context) -> Result<HashSet<String>, String> {
        let subscriptions = self.subscriptions.read().await;

        let subreddits: HashSet<String> = subscriptions
            .by_subreddit
            .keys()
            .map(|x| x.clone())
            .collect();

        Ok(subreddits)
    }

    #[tracing::instrument(skip(self))]
    async fn remove_subreddit(self, _: Context, subreddit: String) -> Result<(), String> {
        info!("Removing subreddit {}", subreddit);
        let coll: mongodb::Collection<Subscription> =
            self.db.database("state").collection("subscriptions");
        let filter = doc! {
            "subreddit": &subreddit,
        };

        coll.delete_many(filter).await.map_err(|e| e.to_string())?;

        let mut subscriptions = self.subscriptions.write().await;

        for sub in subscriptions
            .by_subreddit
            .get(&subreddit)
            .unwrap_or(&HashSet::new())
            .clone()
        {
            subscriptions
                .by_channel
                .get_mut(&sub.channel)
                .ok_or("Tried to remove subscription by channel that already didn't exist!")?
                .remove(&*sub);
        }

        subscriptions.by_subreddit.remove(&subreddit);
        Ok(())
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
    debug!("Starting...");

    initialise_observability!("post-subscriber");

    let _guard = sentry::init((
        "https://d0d89bf871ce425c84eddf6f419dcc7e@o4504774745718784.ingest.us.sentry.io/4508247476600832",
        sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 0.2,
            environment: None,
            server_name: None,
            ..Default::default()
        },
    ));

    let token =
        Token::from_env("DISCORD_TOKEN_BB").expect("Expected DISCORD_TOKEN_BB in the environment");
    let intents = GatewayIntents::empty();
    let mut client_bb = serenity::Client::builder(token, intents)
        .event_handler(Handler)
        .await
        .expect("Err creating client");
    let http_bb = client_bb.http.clone();

    let token =
        Token::from_env("DISCORD_TOKEN_RS").expect("Expected DISCORD_TOKEN_RS in the environment");
    let intents = GatewayIntents::empty();
    let mut client_rs = serenity::Client::builder(token, intents)
        .event_handler(Handler)
        .await
        .expect("Err creating client");
    let http_rs = client_rs.http.clone();

    let mongo_url = env::var("MONGO_URL").expect("MONGO_URL not set");
    let mut client_options = ClientOptions::parse(mongo_url).await.unwrap();
    client_options.app_name = Some("Post Subscriber".to_string());
    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

    let existing_subs = {
        let coll: mongodb::Collection<Subscription> =
            mongodb_client.database("state").collection("subscriptions");

        let mut cursor = coll
            .find(doc! {})
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

    let total_shards_bb: u16 = redis
        .get_int("total_shards_booty-bot")
        .await
        .expect("Failed reading from Redis")
        .map(|x| x as u16)
        .expect("total_shards_booty-bot not set in Redis");

    let total_shards_rs: u16 = redis
        .get_int("total_shards_r-slash")
        .await
        .expect("Failed reading from Redis")
        .map(|x| x as u16)
        .expect("total_shards_r-slash not set in Redis");

    let server = SubscriberServer {
        db: mongodb_client,
        subscriptions,
        discord_bb: http_bb,
        discord_rs: http_rs,
        redis,
        queued_alerts: Arc::new(Mutex::new(VecDeque::new())),
    };

    tokio::spawn(server.clone().watch_alerts());

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
                let server = server.clone();
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
