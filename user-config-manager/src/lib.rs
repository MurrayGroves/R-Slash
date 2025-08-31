use mongodb::Client;
use mongodb::bson::doc;
use mongodb::options::FindOptions;
use serde::{Deserialize, Serialize};
use serenity::all::{ChannelId, GuildId};

use anyhow::Result;

#[derive(Serialize, Deserialize)]
pub struct GuildConfig {
    pub guild_id: GuildId,
    pub star_channel: Option<ChannelId>,
}

impl GuildConfig {
    fn default(guild_id: GuildId) -> GuildConfig {
        GuildConfig {
            guild_id,
            star_channel: None,
        }
    }
}

/// Retrieve a guild config from MongoDB
pub async fn get_guild_config(mongodb: Client, guild_id: GuildId) -> Result<GuildConfig> {
    let collection = mongodb
        .database("config")
        .collection::<GuildConfig>("servers");

    let filter = doc! { "guild_id": guild_id.get().to_string() };

    Ok(match collection.find_one(filter).await? {
        Some(config) => config,
        None => GuildConfig::default(guild_id),
    })
}

/// Save a guild config to MongoDB
pub async fn save_guild_config(mongodb: Client, config: GuildConfig) -> Result<()> {
    let collection = mongodb
        .database("config")
        .collection::<GuildConfig>("servers");

    let filter = doc! {
        "guild_id": config.guild_id.get().to_string()
    };

    match collection.find_one(filter.clone()).await? {
        Some(_) => {
            collection.replace_one(filter, config).await?;
        }
        None => {
            collection.insert_one(config).await?;
        }
    };

    Ok(())
}

#[derive(Serialize, Deserialize, PartialEq, Clone, Copy, Debug)]
pub enum TextAllowLevel {
    /// Text includes link posts
    TextOnly,
    MediaOnly,
    Both,
}

impl Default for TextAllowLevel {
    fn default() -> Self {
        TextAllowLevel::MediaOnly
    }
}

impl TextAllowLevel {
    pub fn allows_for(self, other: TextAllowLevel) -> bool {
        if self == TextAllowLevel::Both {
            true
        } else if self == other {
            true
        } else {
            false
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ChannelConfig {
    pub channel_id: ChannelId,
    pub text_allowed: Option<TextAllowLevel>,
}

impl ChannelConfig {
    fn default(channel_id: ChannelId) -> Self {
        ChannelConfig {
            channel_id,
            text_allowed: None,
        }
    }
}

/// Retrieve a channel config from MongoDB
pub async fn get_channel_config(
    mongodb: &mut Client,
    channel_id: ChannelId,
) -> Result<ChannelConfig> {
    let collection = mongodb
        .database("config")
        .collection::<ChannelConfig>("channels");

    let filter = doc! { "channel_id": channel_id.get().to_string() };

    Ok(match collection.find_one(filter).await? {
        Some(config) => config,
        None => ChannelConfig::default(channel_id),
    })
}

/// Save a channel config to MongoDB
pub async fn save_channel_config(mongodb: &mut Client, config: &ChannelConfig) -> Result<()> {
    let collection = mongodb
        .database("config")
        .collection::<ChannelConfig>("channels");

    let filter = doc! {
        "channel_id": config.channel_id.get().to_string()
    };

    match collection.find_one(filter.clone()).await? {
        Some(_) => {
            collection.replace_one(filter, config).await?;
        }
        None => {
            collection.insert_one(config).await?;
        }
    };

    Ok(())
}
