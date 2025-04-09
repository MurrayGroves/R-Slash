use std::{collections::HashSet, hash::Hash, hash::Hasher};

use mongodb::bson::Bson;
use serde::{Deserialize, Serialize};

#[tarpc::service]
pub trait Subscriber {
    async fn register_subscription(subreddit: String, channel: u64, bot: Bot)
    -> Result<(), String>;

    async fn delete_subscription(subreddit: String, channel: u64, bot: Bot) -> Result<(), String>;

    async fn list_subscriptions(channel: u64, bot: Bot) -> Result<Vec<Subscription>, String>;

    async fn notify(subreddit: String, post_id: String) -> Result<(), String>;

    /// List subreddits that have at least one subscription
    async fn watched_subreddits() -> Result<HashSet<String>, String>;

    /// Remove all subscriptions for a given subreddit (typically done when subreddit is banned)
    async fn remove_subreddit(subreddit: String) -> Result<(), String>;
}

impl serenity::prelude::TypeMapKey for SubscriberClient {
    type Value = SubscriberClient;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub subreddit: String,
    pub channel: u64,
    pub bot: Bot,
    pub added_at: i64,
}

impl PartialEq for Subscription {
    fn eq(&self, other: &Self) -> bool {
        return self.subreddit == other.subreddit
            && self.channel == other.channel
            && self.bot == other.bot;
    }
}

impl Eq for Subscription {}

impl Hash for Subscription {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.subreddit.hash(state);
        self.channel.hash(state);
        self.bot.hash(state);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Hash)]
pub enum Bot {
    BB,
    RS,
}

impl Into<Bson> for Bot {
    fn into(self) -> Bson {
        match self {
            Bot::BB => Bson::String("BB".to_string()),
            Bot::RS => Bson::String("RS".to_string()),
        }
    }
}
