use std::hash::{Hash, Hasher};

use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use serenity::all::ChannelId;
use tokio::time::{Duration, Instant};

#[tarpc::service]
pub trait AutoPoster {
    async fn register_autopost(
        subreddit: String,
        channel: u64,
        interval: Duration,
        limit: Option<u32>,
        search: Option<String>,
        bot: u64,
        interaction_id: u64,
    ) -> Result<i64, String>;

    async fn delete_autopost(id: i64, channel_id: u64) -> Result<PostMemory, String>;

    async fn list_autoposts(channel: u64, bot: u64) -> Result<Vec<PostMemory>, String>;

    async fn ping() -> Result<(), String>;
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

impl From<PostMemoryIntermediate> for PostMemory {
    fn from(value: PostMemoryIntermediate) -> Self {
        Self {
            id: value.id.unwrap_or(value.channel as i64),
            next_post: Instant::now() + Duration::from_secs(value.interval.try_into().unwrap()),
            channel: ChannelId::new(value.channel),
            subreddit: value.subreddit,
            search: value.search,
            interval: Duration::from_secs(value.interval.try_into().unwrap()),
            limit: value.limit,
            current: value.current,
            bot: value.bot.try_into().unwrap(),
        }
    }
}

impl From<PostMemory> for PostMemoryIntermediate {
    fn from(value: PostMemory) -> Self {
        Self {
            channel: value.channel.get(),
            subreddit: value.subreddit,
            search: value.search,
            interval: value.interval.as_secs().try_into().unwrap(),
            limit: value.limit,
            current: value.current,
            bot: value.bot.try_into().unwrap(),
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
    pub interval: i64,
    pub limit: Option<u32>,
    pub current: u32,
    pub bot: i64,
    pub id: Option<i64>,
}

impl PartialEq for PostMemory {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for PostMemory {}

impl Hash for PostMemory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.next_post.hash(state);
        self.current.hash(state);
    }
}

impl Ord for PostMemory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let to_ret = self.next_post.cmp(&other.next_post).reverse();
        to_ret
    }
}

impl PartialOrd for PostMemory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
