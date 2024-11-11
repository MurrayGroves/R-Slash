use futures::{Future, StreamExt};
use mongodb::bson::doc;
use mongodb::options::ClientOptions;
use redis::AsyncCommands;
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, GuildId, Http};
use serenity::json::json;
use serenity::{all::EventHandler, async_trait};

use log::{debug, error, info, warn};
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Instant};

use futures::future;
use opentelemetry::trace::TracerProvider;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{trace, Resource};
use tarpc::{
	context,
	server::{self, Channel},
	tokio_serde::formats::Bincode,
};
use tonic::metadata::MetadataMap;
use tracing_subscriber::{filter, Layer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use post_subscriber::{Bot, Subscriber, Subscription};
use rslash_types::span_filter;

struct Subscriptions {
	by_subreddit: HashMap<String, HashSet<Arc<Subscription>>>,
	by_channel: HashMap<u64, HashSet<Arc<Subscription>>>,
}

struct PostAlert {
	message: CreateMessage,
	subscriptions: Vec<Arc<Subscription>>,
	timestamp: Instant,
}

#[derive(Clone)]
struct SubscriberServer {
	db: Arc<Mutex<mongodb::Client>>,
	subscriptions: Arc<RwLock<Subscriptions>>,
	discord_bb: Arc<Http>,
	discord_rs: Arc<Http>,
	redis: redis::aio::MultiplexedConnection,
	posthog: posthog::Client,
	queued_alerts: Arc<Mutex<VecDeque<PostAlert>>>,
}

impl SubscriberServer {
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

			let mut last_alert = Instant::now() - inner_duration;
			for alert in next_alert.subscriptions {
				// Ensure we don't send more than 35 messages per second
				if last_alert.elapsed() < inner_duration {
					tokio::time::sleep(inner_duration - last_alert.elapsed()).await;
				}
				last_alert = Instant::now();
				let channel: ChannelId = alert.channel.into();
				let http = match alert.bot {
					Bot::BB => &self.discord_bb,
					Bot::RS => &self.discord_rs,
				};

				match channel.send_message(http, next_alert.message.clone()).await {
					Ok(_) => debug!("Sent {} to channel {}, was added {} seconds ago", alert.subreddit, channel, next_alert.timestamp.elapsed().as_secs()),
					Err(e) => {
						if let serenity::Error::Http(e) = e {
							if let serenity::http::HttpError::UnsuccessfulRequest(e) = e {
								if e.error.code == 10003 {
									debug!("Channel doesn't exist anymore, deleting subscription");
									let mut subscriptions = self.subscriptions.write().await;
									match subscriptions.by_channel.get_mut(&alert.channel) {
										Some(x) => {
											x.remove(&*alert);
										}
										None => {
											warn!("Tried to delete subscription by channel that already didn't exist!");
										}
									};
									match subscriptions.by_subreddit.get_mut(&alert.subreddit) {
										Some(x) => {
											x.remove(&*alert);
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
			}
		}
	}
}

impl Subscriber for SubscriberServer {
	#[tracing::instrument(skip(self))]
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
	#[tracing::instrument(skip(self))]
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

		let message: Result<CreateMessage, anyhow::Error> = post.try_into();
		let mut message = match message {
			Ok(x) => x,
			Err(e) => {
				warn!("Failed to convert post to message: {:?}", e);
				return Err(e.to_string());
			}
		};

		// Remove the components because we don't want autopost and refresh options in this context
		message = message.components(vec![]);

		let _ = self
			.posthog
			.capture(
				"subreddit_new_post",
				json!({"subreddit": &subreddit}),
				None::<GuildId>,
				None::<ChannelId>,
				"post-subscriber",
			)
			.await;

		debug!("Filtered subscriptions {:?}", filtered);

		let alert = PostAlert {
			message: message.clone(),
			subscriptions: filtered.into_iter().map(|x| (*x).clone()).collect(),
			timestamp: Instant::now(),
		};

		let mut queued_alerts = self.queued_alerts.lock().await;
		queued_alerts.push_back(alert);

		Ok(())
	}

	#[tracing::instrument(skip(self))]
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

async fn spawn(fut: impl Future<Output=()> + Send + 'static) {
	tokio::spawn(fut);
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {}

#[tokio::main]
async fn main() {
	debug!("Starting...");

	let tracer_provider = opentelemetry_otlp::new_pipeline()
		.tracing()
		.with_exporter(
			opentelemetry_otlp::new_exporter()
				.tonic()
				.with_endpoint("http://100.67.30.19:4317")
		)
		.with_trace_config(
			trace::config().with_resource(Resource::new(vec![opentelemetry::KeyValue::new(
				opentelemetry_semantic_conventions::resource::SERVICE_NAME,
				"post_subscriber".to_string(),
			)])),
		)
		.install_batch(opentelemetry_sdk::runtime::Tokio).unwrap();

	let tracer = tracer_provider.tracer("post_subscriber");

	let telemetry = tracing_opentelemetry::layer().with_tracer(tracer).with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
		span_filter!(meta, cx);
	}));

	tracing_subscriber::Registry::default()
		.with(telemetry)
		.with(sentry::integrations::tracing::layer().with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
			span_filter!(meta, cx);
		})))
		.with(
			tracing_subscriber::fmt::layer()
				.compact()
				.with_ansi(false)
				.with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
					span_filter!(meta, cx);
				}))
		)
		.init();

	let _guard = sentry::init(("https://d0d89bf871ce425c84eddf6f419dcc7e@o4504774745718784.ingest.us.sentry.io/4508247476600832", sentry::ClientOptions {
		release: sentry::release_name!(),
		traces_sample_rate: 0.2,
		environment: None,
		server_name: None,
		..Default::default()
	}));

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

	let total_shards_bb: redis::RedisResult<u32> = redis.get("total_shards_booty-bot").await;
	let total_shards_bb: u32 = total_shards_bb.expect("Failed to get or convert total_shards");

	let total_shards_rs: redis::RedisResult<u32> = redis.get("total_shards_r-slash").await;
	let total_shards_rs: u32 = total_shards_rs.expect("Failed to get or convert total_shards");

	let server = SubscriberServer {
		db: mongodb_client,
		subscriptions,
		discord_bb: http_bb,
		discord_rs: http_rs,
		redis,
		posthog: posthog_client,
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
