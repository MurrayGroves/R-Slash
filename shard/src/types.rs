use std::{collections::HashMap, sync::RwLock};

use serenity::all::CreateActionRow;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}

/// Stores config values required for operation of the shard
#[derive(Debug, Clone)]
pub struct ConfigStruct {
    pub shard_id: u32,
    pub nsfw_subreddits: Vec<String>,
    pub auto_post_chan: tokio::sync::mpsc::Sender<crate::poster::AutoPostCommand>,
    pub redis: redis::aio::MultiplexedConnection,
}

impl serenity::prelude::TypeMapKey for ConfigStruct {
    type Value = ConfigStruct;
}

#[derive(Debug, Clone)]
pub struct InteractionResponse {
    pub file: Option<serenity::builder::CreateAttachment>,
    pub embed: Option<serenity::builder::CreateEmbed>,
    pub content: Option<String>,
    pub ephemeral: bool,
    pub components: Option<Vec<CreateActionRow>>,
    pub fallback: ResponseFallbackMethod,
}

impl Default for InteractionResponse {
    fn default() -> Self {
        InteractionResponse {
            file: None,
            embed: None,
            content: None,
            ephemeral: false,
            components: None,
            fallback: ResponseFallbackMethod::Error
        }
    }
}

// What to do if sending a response fails due to already being acknowledged
#[derive(Debug, Clone)]
pub enum ResponseFallbackMethod {
    /// Edit the original response
    Edit,
    /// Send a followup response
    Followup,
    /// Return an error
    Error,
    /// Do nothing
    None
}


