use std::collections::HashMap;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}

/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    RoleId(serenity::model::prelude::RoleId),
    Bool(bool),
    REDIS(redis::aio::Connection),
    MONGODB(mongodb::Client),
    SubredditList(Vec<String>),
    PosthogClient(posthog::Client),
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
}

impl serenity::prelude::TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
}
