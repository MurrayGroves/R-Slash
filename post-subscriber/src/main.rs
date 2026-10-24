use futures::{Future, StreamExt};
use mongodb::bson::doc;
use mongodb::options::ClientOptions;
use post_api::PostInContext;
use redis::AsyncTypedCommands;
use serenity::Error;
use serenity::all::{
    ChannelId, CreateMessage, GatewayError, GatewayIntents, GenericChannelId, GuildId, Http, Token,
};
use serenity::{all::EventHandler, async_trait};

use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Instant, Interval, interval, timeout};
use tracing::trace;
use user_config_manager::{TextAllowLevel, get_channel_config};

use futures::future;
use metrics::{counter, gauge, histogram};
use post_subscriber::{Bot, Subscriber, Subscription};
use rslash_common::{get_post_content_type, initialise_observability, span_filter};
use tarpc::{server::Channel, tokio_serde::formats::Bincode};

use anyhow::anyhow;
use opentelemetry::global;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::logs;
use tarpc::context::Context;
use tracing_subscriber::{
    EnvFilter, Layer, prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt,
};

use crate::message_sender::MessageSender;

mod message_sender;

struct Subscriptions {
    by_subreddit: HashMap<String, HashSet<Arc<Subscription>>>,
    by_channel: HashMap<GenericChannelId, HashSet<Arc<Subscription>>>,
}

#[derive(Clone)]
struct PostAlert {
    post: PostInContext,
    subscriptions: Vec<Arc<Subscription>>,
    timestamp: Instant,
    text_level: TextAllowLevel,
}

static MESSAGE_SENDER_COUNT: usize = 5;

struct SubscriptionManager {
    subscriptions: Arc<RwLock<Subscriptions>>,
    queued_alerts: Arc<Mutex<VecDeque<PostAlert>>>,
    failed_req_counter: Arc<Mutex<usize>>,
    message_sender_commands:
        [tokio::sync::mpsc::Sender<message_sender::Command>; MESSAGE_SENDER_COUNT],
    message_sender_deletion_requests:
        [tokio::sync::mpsc::Receiver<Arc<Subscription>>; MESSAGE_SENDER_COUNT],
}

impl SubscriptionManager {
    #[tracing::instrument(skip(self))]
    async fn watch_alerts(mut self) {
        let mut outer_interval = interval(tokio::time::Duration::from_millis(100));
        let mut inner_interval = interval(tokio::time::Duration::from_millis(1000 / 20)); // Send alerts at max of 20 per second (Discord global rate limit is 50 reqs per second)
        loop {
            outer_interval.tick().await;
            trace!("Locking queued alerts.");
            let mut queued_alerts = self.queued_alerts.lock().await;
            let next_alert = match queued_alerts.pop_front() {
                Some(x) => x,
                None => continue,
            };
            drop(queued_alerts);
            debug!("Received queued alert.");

            let failed_reqs = self.failed_req_counter.lock().await;
            if *failed_reqs > 1000 {
                error!("Too many failed requests, stopping alert processing for 10 mins");
                drop(failed_reqs);
                tokio::time::sleep(Duration::from_secs(600)).await;
                info!("Resuming alert processing");
                continue;
            }
            drop(failed_reqs);

            counter!("subscriber_processed_alert_batches").increment(1);

            for channel in &self.message_sender_commands {
                channel
                    .send(message_sender::Command::SetAlert(message_sender::Alert {
                        post: next_alert.post.clone(),
                        timestamp: next_alert.timestamp,
                        text_allow_level: next_alert.text_level,
                    }))
                    .await
                    .expect("Message sender's channel was closed, couldn't send to it.");
            }

            let mut sender_idx = 0;
            for alert in &next_alert.subscriptions {
                inner_interval.tick().await;

                let failed_reqs = self.failed_req_counter.lock().await;
                if *failed_reqs > 1000 {
                    break;
                }
                drop(failed_reqs);

                self.message_sender_commands[sender_idx]
                    .send(message_sender::Command::SendToSubcription(alert.clone()))
                    .await
                    .expect("Message sender's channel was closed, couldn't send to it.");
                sender_idx = (sender_idx + 1) % MESSAGE_SENDER_COUNT;
            }

            debug!("Transferred posts to senders");

            for deletion_channel in &mut self.message_sender_deletion_requests {
                while let Ok(subscription) = deletion_channel.try_recv() {
                    debug!("Channel doesn't exist anymore, deleting subscription");
                    let mut subscriptions = self.subscriptions.write().await;
                    match subscriptions.by_channel.get_mut(&subscription.channel) {
                        Some(x) => {
                            x.remove(&*subscription);
                        }
                        None => {
                            warn!(
                                "Tried to delete subscription by channel that already didn't exist!"
                            );
                        }
                    };
                    match subscriptions.by_subreddit.get_mut(&subscription.subreddit) {
                        Some(x) => {
                            x.remove(&*subscription);
                        }
                        None => {
                            warn!("Tried to delete subscription by sub that already didn't exist!");
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone)]
struct SubscriberServer {
    subscriptions: Arc<RwLock<Subscriptions>>,
    queued_alerts: Arc<Mutex<VecDeque<PostAlert>>>,
    db: mongodb::Client,
    redis: redis::aio::MultiplexedConnection,
}

impl Subscriber for SubscriberServer {
    #[tracing::instrument(skip(self))]
    async fn register_subscription(
        self,
        _: Context,
        subreddit: String,
        channel: GenericChannelId,
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
            channel: GenericChannelId::from(channel),
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
        channel: GenericChannelId,
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
            "channel": channel.get() as i64,
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
        channel: GenericChannelId,
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
        debug!("Num subscriptions: {:?}", filtered.len());

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
            post,
            subscriptions: filtered.into_iter().map(|x| (*x).clone()).collect(),
            timestamp: Instant::now(),
            text_level: text_allow_level,
        };
        drop(subscriptions);

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
    println!("Starting...");

    initialise_observability!("post-subscriber");
    info!("Starting!");

    counter!("subscriber_test").increment(2);
    counter!("subscriber_test").increment(2);
    counter!("subscriber_test").increment(2);

    let counter = counter!("subscriber_test2");

    counter.increment(2);
    counter.increment(2);

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

    info!("Connecting to Discord...");
    let token =
        Token::from_env("DISCORD_TOKEN_BB").expect("Expected DISCORD_TOKEN_BB in the environment");
    let intents = GatewayIntents::empty();
    let mut client_bb = match serenity::Client::builder(token, intents)
        .event_handler(Arc::new(Handler))
        .await
    {
        Ok(x) => x,
        Err(e) => match e {
            serenity::Error::Gateway(error) => {
                match error {
                    GatewayError::InvalidAuthentication => {
                        loop {
                            // Sleep forever to prevent startup loops banning us from the API
                            error!("Shard has invalid token!");
                            std::thread::sleep(Duration::from_secs(3600));
                        }
                    }
                    _ => {
                        error!("Error running shard: {:?}", error);
                        panic!("Error running shard: {:?}", error);
                    }
                }
            }
            _ => {
                error!("Error running shard: {:?}", e);
                panic!("Error running shard: {:?}", e)
            }
        },
    };

    let http_bb = client_bb.http.clone();
    info!("Connected for BB");

    let token =
        Token::from_env("DISCORD_TOKEN_RS").expect("Expected DISCORD_TOKEN_RS in the environment");
    let intents = GatewayIntents::empty();
    let mut client_rs = serenity::Client::builder(token, intents)
        .event_handler(Arc::new(Handler))
        .await
        .expect("Err creating client");
    let http_rs = client_rs.http.clone();

    info!("Created Discord clients");

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

    /// Splits an array of channel pairs into arrays of senders and receivers.
    fn unzip_channels<T, const N: usize>(
        pairs: [(Sender<T>, Receiver<T>); N],
    ) -> ([Sender<T>; N], [Receiver<T>; N]) {
        let senders = core::array::from_fn(|i| pairs[i].0.clone());
        (senders, pairs.map(|(_, receiver)| receiver))
    }

    let (command_senders, command_receivers) = unzip_channels(core::array::from_fn(|_| {
        tokio::sync::mpsc::channel::<message_sender::Command>(4096)
    }));
    let (deletion_request_senders, deletion_request_receivers) =
        unzip_channels(core::array::from_fn(|_| {
            tokio::sync::mpsc::channel::<Arc<Subscription>>(4096)
        }));

    let failed_req_counter = Arc::new(Mutex::new(0));

    for (channel, deletion_request_channel) in
        command_receivers.into_iter().zip(deletion_request_senders)
    {
        tokio::spawn(
            MessageSender {
                mongodb: mongodb_client.clone(),
                discord_bb: http_bb.clone(),
                discord_rs: http_rs.clone(),
                failed_req_counter: failed_req_counter.clone(),
                interval: interval(Duration::from_millis(500)),
                channel,
                message: None,
                alert_timestamp: Instant::now(),
                deletion_request_channel,
                text_allow_level: TextAllowLevel::Both,
                post: None,
            }
            .sender_loop(),
        );
    }

    let queued_alerts = Arc::new(Mutex::new(VecDeque::new()));
    let manager = SubscriptionManager {
        subscriptions: subscriptions.clone(),
        queued_alerts: queued_alerts.clone(),
        failed_req_counter: failed_req_counter.clone(),
        message_sender_commands: command_senders,
        message_sender_deletion_requests: deletion_request_receivers,
    };

    let server = SubscriberServer {
        db: mongodb_client,
        subscriptions,
        redis,
        queued_alerts,
    };

    tokio::spawn(manager.watch_alerts());
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            let mut failed_reqs = failed_req_counter.lock().await;
            debug!(
                "Clearing failed request counter of {} requests",
                *failed_reqs
            );
            *failed_reqs = 0;
        }
    });

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    let handle = tokio::spawn(
        listener
            // Ignore accept errors.
            .filter_map(|r| future::ready(r.ok()))
            .map(tarpc::server::BaseChannel::with_defaults)
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
