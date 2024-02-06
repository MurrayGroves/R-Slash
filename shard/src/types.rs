use std::{collections::HashMap, sync::RwLock};

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}

/// Stores config values required for operation of the shard
pub struct ConfigStruct {
    pub shard_id: u32,
    pub nsfw_subreddits: Vec<String>,
    pub auto_post_chan: tokio::sync::mpsc::Sender<crate::poster::AutoPostCommand>,
    pub redis: redis::aio::MultiplexedConnection,
}

impl serenity::prelude::TypeMapKey for ConfigStruct {
    type Value = ConfigStruct;
}