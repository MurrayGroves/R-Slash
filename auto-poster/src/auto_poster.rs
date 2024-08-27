use std::{
    collections::HashSet,
    fmt::Display,
    hash::{Hash, Hasher},
};

use mongodb::bson::Bson;
use serde::{Deserialize, Serialize, Serializer};
use serde_with::{serde_as, DisplayFromStr};
use serenity::all::ChannelId;
use tokio::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[tarpc::service]
pub trait AutoPoster {
    async fn register_autopost(
        subreddit: String,
        channel: ChannelId,
        interval: Duration,
        limit: Option<u32>,
        search: Option<String>,
        bot: u64,
    ) -> Result<u64, String>;

    async fn delete_autopost(id: i64) -> Result<PostMemory, String>;

    async fn list_autoposts(channel: ChannelId, bot: u64) -> Result<Vec<PostMemory>, String>;

    async fn ping() -> Result<(), String>;
}

impl serenity::prelude::TypeMapKey for AutoPosterClient {
    type Value = AutoPosterClient;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "PostMemoryIntermediate", into = "PostMemoryIntermediate")]
pub struct PostMemory {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    pub interval: Duration,
    pub limit: Option<u32>,
    pub current: u32,
    pub bot: u64,
    // Can't serialize instant because it only makes sense in the context of the current runtime - will just reset on startup
    pub next_post: tokio::time::Instant,
    pub id: i64,
}

fn id_serialize<S>(x: &i64, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_some(&Some(x))
}

impl From<PostMemoryIntermediate> for PostMemory {
    fn from(value: PostMemoryIntermediate) -> Self {
        Self {
            id: value.id.unwrap_or(value.channel as i64),
            next_post: Instant::now() + value.interval,
            channel: ChannelId::new(value.channel),
            subreddit: value.subreddit,
            search: value.search,
            interval: value.interval,
            limit: value.limit,
            current: value.current,
            bot: value.bot,
        }
    }
}

impl From<PostMemory> for PostMemoryIntermediate {
    fn from(value: PostMemory) -> Self {
        Self {
            channel: value.channel.get(),
            subreddit: value.subreddit,
            search: value.search,
            interval: value.interval,
            limit: value.limit,
            current: value.current,
            bot: value.bot,
            id: Some(value.id),
        }
    }
}

#[serde_as]
#[derive(Deserialize, Serialize)]
pub struct PostMemoryIntermediate {
    pub channel: u64,
    pub subreddit: String,
    pub search: Option<String>,
    pub interval: Duration,
    pub limit: Option<u32>,
    pub current: u32,
    #[serde_as(as = "serde_with::DisplayFromStr")]
    pub bot: u64,
    pub id: Option<i64>,
}

impl PartialEq for PostMemory {
    fn eq(&self, other: &Self) -> bool {
        return self.id == other.id;
    }
}

impl Eq for PostMemory {}

impl Hash for PostMemory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.subreddit.hash(state);
        self.channel.hash(state);
        self.bot.hash(state);
        self.search.hash(state);
        self.interval.hash(state);
        self.limit.hash(state);
    }
}

impl Ord for PostMemory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.next_post.cmp(&other.next_post).reverse()
    }
}

impl PartialOrd for PostMemory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
