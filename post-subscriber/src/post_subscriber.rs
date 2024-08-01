use std::collections::HashSet;

use mongodb::bson::Bson;
use serde::{Deserialize, Serialize};


#[tarpc::service]
pub trait Subscriber {
    async fn register_subscription(subreddit: String, channel: u64, bot: Bot) -> Result<(), String>;

    async fn delete_subscription(subreddit: String, channel: u64, bot: Bot) -> Result<(), String>;

    async fn list_subscriptions(channel: u64, bot: Bot) -> Result<Vec<Subscription>, String>;

    async fn notify(subreddit: String, post_id: String) -> Result<(), String>;

    async fn watched_subreddits() -> Result<HashSet<String>, String>;
}

impl serenity::prelude::TypeMapKey for SubscriberClient {
    type Value = SubscriberClient;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    pub subreddit: String,
    pub channel: u64,
    pub bot: Bot,
    pub added_at: i64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Bot {
    BB,
    RS
}

impl Into<Bson> for Bot {
    fn into(self) -> Bson {
        match self {
            Bot::BB => Bson::String("BB".to_string()),
            Bot::RS => Bson::String("RS".to_string())
        }
    }
}
