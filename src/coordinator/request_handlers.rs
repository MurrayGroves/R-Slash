//! Contains request handlers for all websocket requests

/// Contains request handlers for requests originating from shards
pub mod shard_request_handlers {
}

/// Contains request handlers for requests originating from downloaders
pub mod downloader_request_handlers {
    use std::{fs, collections::HashMap};
    use serde_json::Value;
    use serde::Serialize;

    /// Returns a String of the JSON list of subreddits from `config.json`
    pub fn get_subreddit_list() -> String {
        let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
        let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
        .expect("config.json is not proper JSON");

        let subreddits = config.get("subreddits").unwrap().as_array().unwrap().clone();
        return serde_json::to_string(&subreddits).unwrap();
    }
}