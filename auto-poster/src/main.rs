#![feature(sync_unsafe_cell)]

use async_recursion::async_recursion;
use chrono::TimeDelta;
use futures::{Future, StreamExt};
use mongodb::bson::{doc, Bson, Document};
use mongodb::options::ClientOptions;
use redis::AsyncCommands;
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http};
use serenity::{all::EventHandler, async_trait};
use tarpc::trace::Context;
use timer::{Guard, Timer};
use tokio::runtime::Handle;
use tokio::time::{Duration, Instant};

use std::cell::{SyncUnsafeCell, UnsafeCell};
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::env;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use tarpc::server::incoming::Incoming;
use tokio::sync::{Mutex, RwLock};

use futures::future::{self, Ready};
use tarpc::{
    client, context,
    server::{self, Channel},
    tokio_serde::formats::Bincode,
};

use anyhow::anyhow;
use tracing::instrument;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{
    prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, Layer,
};

use auto_poster::{AutoPoster, PostMemory};

#[derive(Debug)]
struct UnsafeMemory(SyncUnsafeCell<PostMemory>);

impl Hash for UnsafeMemory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.get().hash(state);
    }
}

impl PartialEq for UnsafeMemory {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Eq for UnsafeMemory {}

impl Ord for UnsafeMemory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.get().cmp(&other.0.get())
    }
}

impl PartialOrd for UnsafeMemory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.0.get().partial_cmp(&other.0.get())
    }
}

impl Deref for UnsafeMemory {
    type Target = PostMemory;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.0.get()) }
    }
}

impl UnsafeMemory {
    pub fn into_inner(self) -> PostMemory {
        self.0.into_inner()
    }

    pub fn new(value: PostMemory) -> Self {
        Self(SyncUnsafeCell::new(value))
    }

    pub fn get_mut(&self) -> &mut PostMemory {
        unsafe { &mut *(self.0.get()) }
    }
}

struct AutoPosts {
    by_channel: HashMap<ChannelId, HashSet<Arc<UnsafeMemory>>>,
    by_id: HashMap<i64, Arc<UnsafeMemory>>,
    queue: BinaryHeap<Arc<UnsafeMemory>>,
}

#[derive(Clone)]
struct AutoPostServer {
    socket_addr: SocketAddr,
    db: Arc<Mutex<mongodb::Client>>,
    autoposts: Arc<RwLock<AutoPosts>>,
    discords: HashMap<u64, Arc<Http>>,
    redis: redis::aio::MultiplexedConnection,
    posthog: posthog::Client,
    timer: Arc<Mutex<Timer>>,
    timer_guard: Arc<Mutex<Option<Guard>>>,
    runtime_handle: Handle,
}

impl AutoPostServer {
    #[instrument(skip(self))]
    pub async fn add_autopost(self, autopost: Arc<UnsafeMemory>) {
        info!("Adding autopost {:?}", autopost);

        let client = self.db.lock().await;
        let coll = client
            .database("state")
            .collection::<PostMemory>("autoposts");
        coll.insert_one(&*autopost.deref().deref())
            .await
            .expect("Failed to insert autopost into database");

        debug!("Inserted autopost into database");

        drop(client);
        let next = (*autopost).next_post;
        let mut autoposts = self.autoposts.write().await;
        autoposts.queue.push(autopost);

        if let Some(first) = autoposts.queue.peek() {
            if first.next_post >= next {
                drop(autoposts);
                self.update_timer().await;
                debug!("Updated timer");
            }
        } else {
            error!("Queue is empty after adding autopost");
        }
        debug!("Finished adding autopost");
    }

    #[instrument(skip(self))]
    pub async fn delete_autopost(self, id: i64) -> Result<PostMemory, String> {
        info!("Deleting autopost {}", id);

        let client = self.db.lock().await;
        let coll: mongodb::Collection<PostMemory> =
            client.database("state").collection("autoposts");

        let filter = doc! {
            "id": id as i64,
        };

        match coll.delete_one(filter).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string()),
        };

        let mut autoposts = self.autoposts.write().await;
        let autopost = match autoposts.by_id.remove(&id) {
            Some(x) => x,
            None => {
                warn!(
                    "Tried to delete autopost with id {} but it doesn't exist",
                    id
                );
                return Err("Autopost doesn't exist".to_string());
            }
        };
        autoposts.queue.retain(|x| x.id != id);
        let by_channel = match autoposts.by_channel.get_mut(&autopost.channel) {
            Some(x) => x,
            None => {
                warn!(
                    "Tried to delete autopost with id {} but channel doesn't exist",
                    id
                );
                return Err("Tried to delete autopost but channel doesn't exist".to_string());
            }
        };
        by_channel.remove(&autopost);
        if by_channel.is_empty() {
            autoposts.by_channel.remove(&autopost.channel);
        }
        if let Some(first) = autoposts.queue.peek() {
            if first.id == id {
                drop(autoposts);
                drop(client);
                self.update_timer().await;
            }
        }

        Ok(Arc::into_inner(autopost)
            .ok_or("Failed to take ownership of PostMemory".to_string())?
            .into_inner())
    }

    #[async_recursion]
    #[instrument(skip(self))]
    async fn update_timer(self) {
        let mut timer_guard = self.timer_guard.lock().await;
        let autoposts = self.autoposts.write().await;
        if let Some(first) = autoposts.queue.peek() {
            let self_clone = self.clone();

            let timer = self.timer.lock().await;

            let duration = first.next_post.duration_since(Instant::now());
            *timer_guard = Some(
                timer.schedule_with_delay(
                    TimeDelta::new(duration.as_secs() as i64, duration.subsec_nanos())
                        .expect("Failed to create TimeDelta"),
                    move || {
                        let mut self_clone = self_clone.clone();
                        self_clone.runtime_handle.clone().spawn(async move {
                            // Send reminder
                            let mut autoposts = self_clone.autoposts.write().await;
                            let autopost = match autoposts.queue.pop() {
                                Some(x) => x,
                                None => {
                                    warn!("Scheduler woke up but found no autopost!");
                                    return;
                                }
                            };

                            debug!("Sending autopost: {:?}", autopost);

                            let message = if let Some(search) = autopost.search.clone() {
                                post_api::get_subreddit_search(
                                    autopost.subreddit.clone(),
                                    search,
                                    &mut self_clone.redis,
                                    autopost.channel,
                                    None,
                                )
                                .await
                            } else {
                                post_api::get_subreddit(
                                    autopost.subreddit.clone(),
                                    &mut self_clone.redis,
                                    autopost.channel,
                                    None,
                                )
                                .await
                            };

                            let channel = autopost.channel.clone();
                            let bot = autopost.bot.clone();

                            (autopost.deref().get_mut()).current += 1;
                            if let Some(limit) = autopost.limit {
                                if autopost.current < limit {
                                    autopost.get_mut().next_post =
                                        autopost.next_post + autopost.interval;
                                    autoposts.queue.push(autopost);
                                } else {
                                    autoposts.by_id.remove(&autopost.id);
                                    match autoposts
                                        .by_channel
                                        .get_mut(&autopost.channel)
                                    {
                                        Some(x) => {
                                            x.remove(&*autopost);
                                            if x.len() == 0 {
                                                autoposts.by_channel.remove(&autopost.channel);
                                            };
                                        },
                                        None => {error!("Tried to get channel to remove autopost from it but channel didn't exist");}
                                    };
                                }
                            } else {
                                autopost.get_mut().next_post =
                                    autopost.next_post + autopost.interval;
                                autoposts.queue.push(autopost);
                            };

                            let message = match message {
                                Ok(x) => x,
                                Err(e) => {
                                    warn!("Error getting subreddit for autopost: {}", e);
                                    // Reschedule timer
                                    if autoposts.queue.peek().is_some() {
                                        drop(autoposts);
                                        self_clone.update_timer().await;
                                    }
                                    return;
                                }
                            };

                            if let Err(why) = {
                                let mut resp = CreateMessage::new();
                                if let Some(embed) = message.embed.clone() {
                                    resp = resp.embed(embed);
                                };

                                if let Some(content) = message.content.clone() {
                                    resp = resp.content(content);
                                };

                                let to_return =
                                    channel.send_message(&self_clone.discords[&bot], resp).await;

                                to_return
                            } {
                                warn!("Error sending message: {:?}", why);
                            }

                            // Reschedule timer
                            if autoposts.queue.peek().is_some() {
                                drop(autoposts);
                                self_clone.update_timer().await;
                            }
                        });
                    },
                ),
            );
        }
    }
}

impl AutoPoster for AutoPostServer {
    #[instrument(skip(self))]
    async fn register_autopost(
        self,
        _: tarpc::context::Context,
        subreddit: String,
        channel: u64,
        interval: Duration,
        limit: Option<u32>,
        search: Option<String>,
        bot: u64,
    ) -> Result<u64, String> {
        println!(
            "Registering autopost for subreddit {} in channel {}",
            subreddit, channel
        );

        let mut memory = PostMemory {
            subreddit,
            channel: ChannelId::new(channel),
            interval,
            limit,
            search,
            bot,
            current: 0,
            next_post: tokio::time::Instant::now() + interval,
            id: 0,
        };

        let mut hasher = DefaultHasher::new();
        memory.hash(&mut hasher);
        let id = hasher.finish();
        memory.id = id as i64;

        let mut autoposts = self.autoposts.write().await;
        let memory = Arc::new(UnsafeMemory::new(memory));
        autoposts.by_id.insert(memory.id, memory.clone());
        autoposts
            .by_channel
            .entry(channel.into())
            .or_insert_with(HashSet::new)
            .insert(memory.clone());

        drop(autoposts);
        self.add_autopost(memory).await;

        Ok(id)
    }

    #[instrument(skip(self))]
    async fn delete_autopost(
        self,
        _: tarpc::context::Context,
        id: i64,
    ) -> Result<PostMemory, String> {
        self.delete_autopost(id).await
    }

    #[instrument(skip(self))]
    async fn list_autoposts(
        self,
        _: ::tarpc::context::Context,
        channel: u64,
        bot: u64,
    ) -> Result<Vec<PostMemory>, String> {
        info!("Listing autoposts for {}", channel);
        let autoposts = self.autoposts.read().await;
        let to_return = match autoposts.by_channel.get(&channel.into()) {
            Some(x) => x
                .iter()
                .filter(|x| x.bot == bot)
                .map(|x| (**x).clone())
                .collect(),
            None => return Ok(vec![]),
        };

        debug!("Returning {:?}", to_return);

        Ok(to_return)
    }

    #[instrument(skip(self))]
    async fn ping(self, _: tarpc::context::Context) -> Result<(), String> {
        info!("Ping received");
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

    tracing_subscriber::Registry::default()
        .with(
            sentry::integrations::tracing::layer()
                .span_filter(|md| {
                    if md.name().contains("recv")
                        || md.name().contains("recv_event")
                        || md.name().contains("dispatch")
                        || md.name().contains("handle_event")
                        || md.name().contains("check_heartbeat")
                        || md.name().contains("headers")
                    {
                        return false;
                    } else {
                        return true;
                    }
                })
                .event_filter(|md| {
                    let level_filter = match md.level() {
                        &tracing::Level::ERROR => sentry::integrations::tracing::EventFilter::Event,
                        &tracing::Level::WARN => sentry::integrations::tracing::EventFilter::Event,
                        &tracing::Level::TRACE => {
                            sentry::integrations::tracing::EventFilter::Ignore
                        }
                        _ => sentry::integrations::tracing::EventFilter::Breadcrumb,
                    };

                    if (!md.target().contains("auto_poster") && !md.target().contains("post_api"))
                        || md.name().contains("serenity")
                        || md.target().contains("serenity")
                    {
                        return sentry::integrations::tracing::EventFilter::Ignore;
                    } else {
                        return level_filter;
                    }
                }),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
                    if meta.name().contains("serenity") {
                        return false;
                    };
                    true
                })),
        )
        .init();

    let _guard = sentry::init(("https://07bd85d599d280093efda1eb9bd65a3c@o4504774745718784.ingest.us.sentry.io/4507850989436928", sentry::ClientOptions {
        release: sentry::release_name!(),
        traces_sample_rate: 0.2,
        environment: None,
        server_name: None,
        ..Default::default()
    }));

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
        let coll: mongodb::Collection<PostMemory> =
            client.database("state").collection("autoposts");

        let mut cursor = coll
            .find(Document::new())
            .await
            .expect("Failed to get autoposts");
        let mut subscriptions = Vec::new();

        while let Some(sub) = cursor.next().await {
            match sub {
                Ok(sub) => {
                    subscriptions.push(sub);
                }
                Err(e) => {
                    error!("Failed to get autoposts: {:?}", e);
                    break;
                }
            }
        }

        subscriptions
    };

    let mut by_channel = HashMap::new();
    let mut by_id = HashMap::new();
    let mut queue = BinaryHeap::new();

    for mut sub in existing_subs {
        sub.next_post = sub.next_post + sub.interval;
        let sub = Arc::new(UnsafeMemory::new(sub));
        by_channel
            .entry(sub.channel.clone())
            .or_insert_with(HashSet::new)
            .insert(sub.clone());
        by_id.insert(sub.id, sub.clone());
        queue.push(sub);
    }

    let redis_url = env::var("REDIS_URL").expect("REDIS_URL not set");
    let redis_client = redis::Client::open(redis_url).unwrap();
    let mut redis = redis_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis");

    let bots = HashMap::from([
        (282921751141285888, "r-slash"),
        (278550142356029441, "booty-bot"),
        (291255986742624256, "testing"),
    ]);

    let mut discord_clients = HashMap::new();
    let mut discord_https = HashMap::new();
    let mut total_shards = HashMap::new();
    for bot in bots.keys() {
        let token =
            env::var(format!("DISCORD_TOKEN_{}", bot.to_string().to_uppercase())).expect(&format!(
                "Expected DISCORD_TOKEN_{} in the environment",
                bot.to_string().to_uppercase()
            ));

        let intents = GatewayIntents::empty();
        let client = serenity::Client::builder(&token, intents)
            .event_handler(Handler)
            .await
            .expect("Err creating client");

        discord_https.insert(bot.clone(), client.http.clone());
        discord_clients.insert(bot.clone(), client);
        let total_shards_this: redis::RedisResult<u32> =
            redis.get(format!("total_shards_{}", bots[bot])).await;

        total_shards.insert(
            bot.clone(),
            total_shards_this.expect("Failed to get or convert total_shards"),
        );
    }

    let autoposts = Arc::new(RwLock::new(AutoPosts {
        by_id,
        by_channel,
        queue,
    }));

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);

    let timer = Arc::new(Mutex::new(Timer::new()));
    let timer_guard = Arc::new(Mutex::new(None));
    let handle = tokio::spawn(
        listener
            // Ignore accept errors.
            .filter_map(|r| future::ready(r.ok()))
            .map(server::BaseChannel::with_defaults)
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated SubscriberServer trait.
            .map(move |channel| {
                let server = AutoPostServer {
                    socket_addr: channel.transport().peer_addr().unwrap(),
                    db: Arc::clone(&mongodb_client),
                    autoposts: Arc::clone(&autoposts),
                    discords: discord_https.clone(),
                    redis: redis.clone(),
                    posthog: posthog_client.clone(),
                    timer: Arc::clone(&timer),
                    timer_guard: Arc::clone(&timer_guard),
                    runtime_handle: Handle::current(),
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

    for (bot, mut client) in discord_clients {
        let total_shards = total_shards[&bot];
        tokio::spawn(async move {
            if let Err(why) = client.start_shard(0, total_shards).await {
                error!("Client error: {:?}", why);
            }
        });
    }

    handle.await.expect("Failed to run server");
    error!("Server stopped!");
}
