use futures::{Future, StreamExt};
use mongodb::bson::doc;
use mongodb::options::ClientOptions;
use serde_derive::{Deserialize, Serialize};
use serenity::all::{ChannelId, CreateMessage, GatewayIntents, Http};
use serenity::{all::EventHandler, async_trait};

use log::{debug, error, info, warn};
use tarpc::server::incoming::Incoming;
use tokio::sync::Mutex;
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


#[tarpc::service]
trait Subscriber {
    async fn register_subscription(subreddit: String, channel: u64) -> Result<(), String>;

    async fn delete_subscription(subreddit: String, channel: u64) -> Result<(), String>;

    async fn list_subscriptions(channel: u64) -> Result<Vec<Subscription>, String>;

    async fn notify(subreddit: String, post_id: String) -> Result<(), String>;

    async fn watched_subreddits() -> Result<HashSet<String>, String>;
}

#[derive(Clone)]
struct SubscriberServer {
    socket_addr: SocketAddr,
    db: Arc<Mutex<mongodb::Client>>,
    subscriptions: Arc<Mutex<Vec<Subscription>>>,
    discord: Arc<Http>,
    redis: redis::aio::MultiplexedConnection,
}

impl Subscriber for SubscriberServer {
    async fn register_subscription(self, _: context::Context, subreddit: String, channel: u64) -> Result<(), String> {
        info!("Registering subscription for subreddit {} to channel {}", subreddit, channel);

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let subscription = Subscription {
            subreddit: subreddit,
            channel: channel
        };

        match coll.insert_one(&subscription, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string())
        };

        self.subscriptions.lock().await.push(subscription);
        Ok(())
    }

    async fn delete_subscription(self, _: context::Context, subreddit: String, channel: u64) -> Result<(), String> {
        info!("Deleting subscription for subreddit {} to channel {}", subreddit, channel);

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let filter = doc! {
            "subreddit": &subreddit,
            "channel": channel as i64
        };

        match coll.delete_one(filter, None).await {
            Ok(_) => (),
            Err(e) => return Err(e.to_string())
        
        };
        self.subscriptions.lock().await.retain(|sub| sub.subreddit != subreddit || sub.channel != channel);
        Ok(())
    }

    // List subscriptions for a channel
    async fn list_subscriptions(self, _: context::Context, channel: u64) -> Result<Vec<Subscription>, String> {
        info!("Listing subscriptions");

        let client = self.db.lock().await;
        let coll: mongodb::Collection<Subscription> = client.database("state").collection("subscriptions");

        let filter = doc!{
            "channel": channel as i64
        };

        let mut cursor = match coll.find(filter, None).await {
            Ok(cursor) => cursor,
            Err(e) => return Err(e.to_string())
        };
        let mut subscriptions = Vec::new();

        while let Some(sub) = cursor.next().await {
            subscriptions.push(sub.unwrap());
        }

        Ok(subscriptions)
    }

    async fn notify(self, _: context::Context, subreddit: String, post_id: String) -> Result<(), String> {
        info!("Notifying subscribers of post {} in subreddit {}", post_id, subreddit);

        let subscriptions = self.subscriptions.lock().await;
        let filtered = subscriptions.iter().filter(|sub| sub.subreddit == subreddit).collect::<Vec<&Subscription>>();

        let mut redis = self.redis.clone();

        let mut post = match post_api::get_post_by_id(&post_id, None, &mut redis, None).await {
            Ok(post) => post,
            Err(e) => return Err(e.to_string())
        };

        // Remove the components because we don't want autopost and refresh options in this context
        post.components = None;

        for sub in filtered {
            info!("Notifying channel {} of post {} in subreddit {}", sub.channel, post_id, subreddit);

            let post = post.clone();

            let channel: ChannelId = sub.channel.into();
            
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

            match channel.send_message(&self.discord, resp).await {
                Ok(_) => (),
                Err(e) => return Err(e.to_string())
            }
        }

        Ok(())
    }

    async fn watched_subreddits(self, _: context::Context) -> Result<HashSet<String>, String> {
        let subscriptions = self.subscriptions.lock().await;
        let subreddits: HashSet<String> = subscriptions.iter().map(|sub| sub.subreddit.clone()).collect();

        info!("Listing watched subreddits {:?}", subreddits);

        Ok(subreddits)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Subscription {
    subreddit: String,
    channel: u64
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

    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");
    // Set gateway intents, which decides what events the bot will be notified about
    let intents = GatewayIntents::empty();
    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some("Post Subscriber".to_string());
    let mongodb_client = Arc::new(Mutex::new(mongodb::Client::with_options(client_options).unwrap()));

    let subscriptions = Arc::new(Mutex::new(Vec::new()));

    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let redis = db_client.get_multiplexed_async_connection().await.expect("Can't connect to redis");

    let mut client =
    serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");

    let http = client.http.clone();

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default).await.unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    tokio::spawn(listener
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
                discord: Arc::clone(&http),
                redis: redis.clone()
            };
            channel.execute(server.serve()).for_each(spawn)
        })
        // Max 10 channels.
        .buffer_unordered(10)
        .for_each(|_| async {})
    );

    info!("Started tarpc server, connecting to discord...");

    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}