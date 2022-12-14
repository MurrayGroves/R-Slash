use log::*;
use std::fs;
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::env;

use serenity::model::prelude::RoleId;
use serenity::builder::CreateEmbed;
use serenity::model::id::{ChannelId, GuildId, UserId};
use serenity::model::gateway::GatewayIntents;
use serenity::{
    async_trait,
    model::gateway::Ready,
    prelude::*,
};
use serenity::model::application::interaction::application_command::ApplicationCommandInteraction;
use serenity::model::application::interaction::InteractionResponseType;
use serenity::model::application::interaction::Interaction;

use tokio::time::{sleep, Duration};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use std::fs::File;
use std::path::Path;
use futures_util::TryStreamExt;
use redis::AsyncCommands;

use redis;
use redis::{Commands, from_redis_value};
use mongodb::options::ClientOptions;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;

use serenity::client::bridge::gateway::event::ShardStageUpdateEvent;
use serenity::model::application::command::CommandOptionType;
use serenity::model::application::component::ButtonStyle;
use serenity::model::guild::{Guild, UnavailableGuild};
use serenity::utils::Colour;


/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    RoleId(RoleId),
    Bool(bool),
    REDIS(redis::Connection),
    MONGODB(mongodb::Client),
    SubredditList(Vec<String>),
    PosthogClient(posthog::Client),
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
}

impl TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
}


#[derive(Debug, Clone)]
pub struct FakeEmbed {
    title: Option<String>,
    description: Option<String>,
    url: Option<String>,
    color: Option<Colour>,
    footer: Option<String>,
    image: Option<String>,
    thumbnail: Option<String>,
    author: Option<String>,
    timestamp: Option<u64>,
    fields: Option<Vec<(String, String, bool)>>,
    buttons: Option<Vec<FakeButton>>
}

#[derive(Debug, Clone)]
pub struct FakeButton {
    label: String,
    style: ButtonStyle,
    url: Option<String>,
    custom_id: Option<String>,
    disabled: bool,
}


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}


async fn capture_event(data: &mut HashMap<String, ConfigValue>, event: &str, properties: Option<HashMap<&str, String>>, distinct_id: &str) {
    let posthog_client = match data.get_mut("posthog_client").unwrap() {
        ConfigValue::PosthogClient(client) => client,
        _ => panic!("posthog_client is not a PosthogClient"),
    };

    let mut properties_map = serde_json::Map::new();
    if properties.is_some() {
        for (key, value) in properties.unwrap() {
            properties_map.insert(key.to_string(), serde_json::Value::String(value));
        }
    }

    debug!("{:?}", posthog_client.capture(event, properties_map, distinct_id).await.unwrap().text().await.unwrap());
}

async fn get_subreddit_cmd(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, ctx: &Context) -> FakeEmbed {
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    let nsfw_subreddits = match data_mut.get_mut("nsfw_subreddits").unwrap() {
        ConfigValue::SubredditList(list) => list,
        _ => panic!("nsfw_subreddits is not a list"),
    };

    let nsfw_subreddits = nsfw_subreddits.clone();

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.unwrap();
    let subreddit = subreddit.as_str().unwrap().to_string();

    capture_event(data_mut, "subreddit_cmd", Some(HashMap::from([("subreddit", subreddit.clone())])), &format!("user_{}", command.user.id.0.to_string())).await;

    let mut con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    if nsfw_subreddits.contains(&subreddit) {
        let channel = command.channel_id.to_channel_cached(&ctx.cache).unwrap();
        if !channel.is_nsfw() {
            return FakeEmbed {
                title: Some("NSFW subreddits can only be used in NSFW channels".to_string()),
                author: None,
                timestamp: None,
                description: Some("Discord requires NSFW content to only be sent in NSFW channels, find out how to fix this [here](https://support.discord.com/hc/en-us/articles/115000084051-NSFW-Channels-and-Content)".to_string()),
                url: None,
                color: Some(Colour::from_rgb(255, 0, 0)),
                footer: None,
                image: None,
                thumbnail: None,
                fields: None,
                buttons: None
            }
        }
    }

    return get_subreddit(subreddit, &mut con, command.channel_id).await
}

async fn get_subreddit(subreddit: String, con: &mut redis::Connection, channel: ChannelId) -> FakeEmbed {
    let mut index: u16 = con.incr(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 1i16).unwrap();
    let _:() = con.expire(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 60*60).unwrap();
    index -= 1;
    let length: u16 = con.llen(format!("subreddit:{}:posts", &subreddit)).unwrap();
    index = length - (index + 1);

    if index >= length {
        let _:() = con.set(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 0i16).unwrap();
        index = 0;
    }


    let post: Vec<String> = redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(index).arg(index).query(con).unwrap();
    let post: HashMap<String, redis::Value> = con.hgetall(&post[0]).unwrap();

    let embed = FakeEmbed {
        title: Some(from_redis_value(&post.get("title").unwrap().clone()).unwrap()),
        description: None,
        author: Some(from_redis_value(&post.get("author").unwrap().clone()).unwrap()),
        url: Some(from_redis_value(&post.get("url").unwrap().clone()).unwrap()),
        color: Some(Colour::from_rgb(0, 255, 0)),
        footer: None,
        image: Some(from_redis_value(&post.get("embed_url").unwrap().clone()).unwrap()),
        thumbnail: None,
        timestamp: Some(from_redis_value(post.get("timestamp").unwrap()).unwrap()),
        fields: None,
        buttons: Some(vec![FakeButton {
            label: "🔁".to_string(),
            style: ButtonStyle::Primary,
            url: None,
            custom_id: Some(format!("subreddit:{}", subreddit)),
            disabled: false
        }])
    };

    return embed;
}

fn error_embed(code: &str) -> FakeEmbed {
    return FakeEmbed {
        title: Some("An Error Occurred".to_string()),
        description: Some(format!("Please report this in the support server.\n Error: {}", code)),
        author: None,
        url: None,
        color: Some(Colour::from_rgb(255, 0,0)),
        footer: None,
        image: None,
        thumbnail: None,
        timestamp: None,
        fields: None,
        buttons: None
    };
}


async fn update_guild_commands(guild_id: GuildId, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, ctx: &Context) {
    let mongodb_client = match data.get_mut::<ConfigStruct>().unwrap().get_mut("mongodb_connection").unwrap() {
        ConfigValue::MONGODB(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("settings");

    let filter = doc! {"id": "subreddit_list".to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    let doc = cursor.try_next().await.unwrap().unwrap();
    let sfw_subreddits: Vec<&str> = doc.get_array("sfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();
    let nsfw_subreddits: Vec<&str> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();


    let guild_config = fetch_guild_config(guild_id, data).await;
    let nsfw = guild_config.get_str("nsfw").unwrap();

    let _ = guild_id.create_application_command(&ctx.http, |command| {
    command
        .name("get")
        .description("Get a post from a specified subreddit");

    command.create_option(|option| {
        option.name("subreddit")
            .description("The subreddit to get a post from")
            .required(true)
            .kind(CommandOptionType::String);

        if nsfw == "nsfw" || nsfw == "both" {
            for subreddit in &nsfw_subreddits {
                option.add_string_choice(subreddit, subreddit);
            }
        }
        if nsfw == "non-nsfw" || nsfw == "both" {
            for subreddit in &sfw_subreddits {
                option.add_string_choice(subreddit, subreddit);
            }
        }

        return option;
    });

    return command;
    }).await.expect("Failed to register slash commands");

}


/// Fetches a guild's configuration, creating it if it doesn't exist.
async fn fetch_guild_config(guild_id: GuildId, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>) -> Document {
    let mongodb_client = match data.get_mut::<ConfigStruct>().unwrap().get_mut("mongodb_connection").unwrap() {
        ConfigValue::MONGODB(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("servers");

    let filter = doc! {"id": guild_id.0.to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    return match cursor.try_next().await.unwrap() {
        Some(doc) => doc, // If guild configuration does exist
        None => { // If guild configuration doesn't exist
            let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");

            // Check if booty bot or r slash
            let nsfw = match application_id {
                278550142356029441 => "nsfw",
                _ => "non-nsfw"
            };

            let server = doc! {
                "id": guild_id.0.to_string(),
                "nsfw": nsfw,
            };

            coll.insert_one(server, None).await.unwrap();
            let mut cursor = coll.find(filter, find_options).await.unwrap();
            cursor.try_next().await.unwrap().unwrap()
        }
    };
}

async fn set_guild_config(old_config: Document, config: Document, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>) {
    let mongodb_client = match data.get_mut::<ConfigStruct>().unwrap().get_mut("mongodb_connection").unwrap() {
        ConfigValue::MONGODB(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("servers");
    coll.replace_one(old_config, config, None).await.unwrap();
}

#[derive(Debug)]
pub struct MembershipTier {
    _name: String,
    active: bool,
}

#[derive(Debug)]
pub struct MembershipTiers {
    bronze: MembershipTier,
}

// Fetch a user's membership tiers from MongoDB. Returns current status, name of tier, when the user first had the membership, and when the membership expires.
async fn get_user_tiers(user: UserId, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>) -> MembershipTiers {
    let mongodb_client = match data.get_mut::<ConfigStruct>().unwrap().get_mut("mongodb_connection").unwrap() {
        ConfigValue::MONGODB(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user.0.to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    let doc = match cursor.try_next().await.unwrap() {
        Some(doc) => doc, // If user information exists, return it
        None => { // If user information doesn't exist, return a document that indicates no active memberships.
            doc! {
                "active": []
            }
        }
    };

    debug!("User document: {:?}", doc);

    let bronze = doc.get_array("active").unwrap().contains(&mongodb::bson::Bson::from("bronze"));

    return MembershipTiers {
        bronze: MembershipTier {
            _name: "bronze".to_string(),
            active: bronze,
        },
    };
}


async fn cmd_get_user_tiers(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, _ctx: &Context) -> FakeEmbed {
    let tiers = get_user_tiers(command.user.id, data).await;
    debug!("Tiers: {:?}", tiers);

    let bronze = match tiers.bronze.active {
        true => "Active",
        false => "Inactive",
    }.to_string();

    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    capture_event(data_mut, "cmd_get_user_tiers", Some(HashMap::from([("bronze_active", bronze.to_string())])), &format!("user_{}", command.user.id.0.to_string())).await;

    return FakeEmbed {
        title: Some("Your membership tiers".to_string()),
        description: Some("Get Premium here: https://ko-fi.com/rslash".to_string()),
        url: None,
        fields: Some(vec![
            ("Premium".to_string(), bronze, false),
        ]),
        author: None,
        timestamp: None,
        footer: None,
        image: None,
        color: None,
        thumbnail: None,
        buttons: None
    };
}


async fn get_custom_subreddit(command: &ApplicationCommandInteraction, ctx: &Context) -> FakeEmbed {
    let mut data = ctx.data.write().await;

    let membership = get_user_tiers(command.user.id, &mut data).await;
    if !membership.bronze.active {
        return FakeEmbed {
            title: Some("Premium Feature".to_string()),
            description: Some("You must have premium in order to use this command.
            Get it here: https://ko-fi.com/rslash".to_string()),
            url: None,
            color: Some(Colour::from_rgb(255, 0, 0)),
            footer: None,
            image: None,
            thumbnail: None,
            author: None,
            timestamp: None,
            fields: None,
            buttons: None
        }
    }

    drop(data); // Release lock while performing web request

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.unwrap();
    let subreddit = subreddit.as_str().unwrap().to_string();

    let web_client = reqwest::Client::new();
    let res = match web_client
        .get(format!("https://reddit.com/r/{}.json", subreddit))
        .send()
        .await {
            Ok(x) => x,
            Err(y) => {
                warn!("Reddit request failed: {}", y);
                return error_embed("AHWUHABA&AWOPL");
            }
    };

    if res.status() != 200 {
        return FakeEmbed {
            title: Some("Subreddit Inaccessible".to_string()),
            description: Some(format!("r/{} is private or does not exist.", subreddit).to_string()),
            url: None,
            color: Some(Colour::from_rgb(255, 0, 0)),
            footer: None,
            image: None,
            thumbnail: None,
            author: None,
            timestamp: None,
            fields: None,
            buttons: None
        }
    }

    let mut data = ctx.data.write().await;
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    let con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let last_cached: i64 = con.get(&format!("{}", subreddit)).unwrap_or(0);

    if last_cached == 0 {
        let post: HashMap<String, redis::Value>;
        debug!("Subreddit last cached more than an hour ago, updating...");
        command.defer(&ctx.http).await.unwrap_or_else(|e| {
            warn!("Failed to defer response: {}", e);
        });
        let _:() = con.lpush("custom_subreddits_queue", subreddit.clone()).unwrap();
        drop(data); // Release lock while waiting to avoid deadlocks.
        loop {
            sleep(Duration::from_millis(1000)).await;
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();

            let con = match data_mut.get_mut("redis_connection").unwrap() {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(0),
            }.unwrap();

            let posts: Vec<String> = match redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(0i64).query(con) {
                Ok(posts) => {
                    posts
                },
                Err(_) => {
                    continue;
                }
            };
            if posts.len() > 0 {
                post = con.hgetall(&posts[0]).unwrap();
                break;
            }
        }

        return FakeEmbed {
            title: Some(from_redis_value(&post.get("title").unwrap().clone()).unwrap()),
            description: None,
            author: Some(from_redis_value(&post.get("author").unwrap().clone()).unwrap()),
            url: Some(from_redis_value(&post.get("url").unwrap().clone()).unwrap()),
            color: Some(Colour::from_rgb(0, 255, 0)),
            footer: None,
            image: Some(from_redis_value(&post.get("embed_url").unwrap().clone()).unwrap()),
            thumbnail: None,
            timestamp: Some(from_redis_value(post.get("timestamp").unwrap()).unwrap()),
            fields: None,
            buttons: Some(vec![FakeButton {
                label: "🔁".to_string(),
                style: ButtonStyle::Primary,
                url: None,
                custom_id: Some(format!("subreddit:{}", subreddit)),
                disabled: false
            }])
        };
    } else if last_cached +  3600000 < get_epoch_ms() as i64 {
        // Tell downloader to update the subreddit, but use outdated posts for now.
        let _:() = con.lpush("custom_subreddits_queue", subreddit.clone()).unwrap();
    }

    return get_subreddit_cmd(command, &mut data, ctx).await;
}


async fn info(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, ctx: &Context) -> FakeEmbed {
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    capture_event(data_mut, "cmd_info", None, &format!("user_{}", command.user.id.0.to_string())).await;

    let con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(0),
    }.unwrap();

    let guild_counts: HashMap<String, redis::Value> = con.hgetall(format!("shard_guild_counts_{}", get_namespace().await)).unwrap();
    let mut guild_count = 0;
    for (_, count) in guild_counts {
        guild_count += from_redis_value::<u64>(&count).unwrap();
    }

    return FakeEmbed {
        title: Some("Info".to_string()),
        description: Some("[Support Server](https://discord.gg/jYtCFQG)\n\n[Add me to a server](https://discord.com/api/oauth2/authorize?client_id=278550142356029441&permissions=515463498752&scope=applications.commands%20bot)".to_string()),
        url: None,
        color: Some(Colour::from_rgb(0, 255, 0)),
        footer: None,
        image: None,
        thumbnail: None,
        author: None,
        timestamp: None,
        fields: Some(vec![
            ("Servers".to_string(), guild_count.to_string(), true),
            ("Shard ID".to_string(), ctx.shard_id.to_string(), true),
            ("Shard Count".to_string(), ctx.cache.shard_count().to_string(), true),
        ]),
        buttons: None
    }
}


async fn configure_server(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, ctx: &Context) -> FakeEmbed {
    debug!("{:?}", command.data.options);

    let mut embed = FakeEmbed {
        title: None,
        description: None,
        author: None,
        url: None,
        color: None,
        footer: None,
        image: None,
        thumbnail: None,
        timestamp: None,
        fields: None,
        buttons: None
    };

    match command.data.options[0].name.as_str() {
        "commands" => {
            match command.data.options[0].options[0].name.as_str() {
                "nsfw" => {
                    let mut guild = fetch_guild_config(command.guild_id.unwrap(), data).await;
                    let old_config = guild.clone();
                    let nsfw = command.data.options[0].options[0].options[0].value.clone().unwrap().to_string().replace('"', "");
                    guild.insert("nsfw", &nsfw);

                    set_guild_config(old_config, guild, data).await;


                    embed.title = Some("Server Configuration Changed".to_string());
                    embed.description = Some(format!("The server's nsfw configuration has been changed to {}", &nsfw));
                    embed.color = Some(Colour::from_rgb(0, 255, 0));

                    update_guild_commands(command.guild_id.unwrap(), data, ctx).await;
                },
                _ => {
                    embed = error_embed("11615");
                }

            };
        },
        _ => {
            embed = error_embed("171651");
        }
    }


    return embed;
}

macro_rules! fake_embed_to_embed {
    ($a:expr) => {
        |e: &mut CreateEmbed| {
            if $a.timestamp.is_some() {
                e.timestamp(serenity::model::timestamp::Timestamp::from_unix_timestamp($a.timestamp.unwrap() as i64).unwrap());
            }

            if $a.footer.is_some() {
                let text = $a.footer.unwrap();
                e.footer(|footer| {
                    footer.text(text)
                });
            }

            if $a.fields.is_some() {
                e.fields($a.fields.unwrap());
            }

            if $a.color.is_some() {
                e.colour($a.color.unwrap());
            }

            if $a.description.is_some() {
                e.description($a.description.unwrap());
            }

            if $a.title.is_some() {
                e.title($a.title.unwrap());
            }

            if $a.url.is_some() {
                e.url($a.url.unwrap().clone());
            }

            if $a.author.is_some() {
                let name = $a.author.unwrap();
                e.author(|author| {
                    author.name(&name)
                        .url(format!("https://reddit.com/u/{}", name))
                });
            }

            if $a.thumbnail.is_some() {
                e.thumbnail($a.thumbnail.unwrap());
            }

            if $a.image.is_some() {
                let url = $a.image.clone().unwrap();
                if !(url.contains("imgur") && url.contains(".gif")) && !(url.contains("redgifs")) {
                    e.image(url);
                }
            }

            return e;
        }
    }
}

macro_rules! fake_embed_to_buttons {
    ($a:expr) => {
        |c| {
            c.create_action_row(|a| {
                for button in $a.clone().buttons.unwrap() {
                    a.create_button(|b| {
                        b.label(button.label)
                            .style(button.style)
                            .disabled(button.disabled);

                        if button.url.is_some() {
                            b.url(button.url.unwrap());
                        }

                        if button.custom_id.is_some() {
                         b.custom_id(button.custom_id.unwrap());
                        }

                        return b;
                    });
                }

                return a;
            })
        }
    };
}

macro_rules! fake_embed_to_message {
    ($a:expr) => {
        |message| {
            let mut do_buttons = true;
            if $a.url.is_some() {
                let url = $a.image.clone().unwrap();
                if (url.contains("imgur") && url.contains(".gif") ) || url.contains("redgifs") {
                    do_buttons = false;
                }
            }
    
            if ($a.buttons.is_some()) && do_buttons {
                message.components(fake_embed_to_buttons!($a));
            }
    
            message.embed(fake_embed_to_embed!($a))
        }
    };
}


/// Discord event handler
struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client receives new data about a guild
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: bool) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();

        if is_new { // First time client has seen the guild
            capture_event(data_mut, "guild_join", None, &format!("guild_{}", guild.id.0.to_string())).await;
            update_guild_commands(guild.id, &mut ctx.data.write().await, &ctx).await;
        }

        let con = match data_mut.get_mut("redis_connection").unwrap() {
            ConfigValue::REDIS(db) => Ok(db),
            _ => Err(0),
        }.unwrap();

        let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace().await), ctx.shard_id, ctx.cache.guild_count()).unwrap();
    }

    async fn guild_delete(&self, ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();

        capture_event(data_mut, "guild_leave", None, &format!("guild_{}", incomplete.id.0.to_string())).await;

        let con = match data_mut.get_mut("redis_connection").unwrap() {
            ConfigValue::REDIS(db) => Ok(db),
            _ => Err(0),
        }.unwrap();

        let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace().await), ctx.shard_id, ctx.cache.guild_count()).unwrap();
    }

    async fn message(&self, ctx: Context, new_message: serenity::model::channel::Message) {
        if new_message.content != "" {
            debug!("{}: {}", new_message.author.name, new_message.content);
            if new_message.content.contains("<@278550142356029441>") {
                update_guild_commands(new_message.guild_id.unwrap(), &mut ctx.data.write().await, &ctx).await;
            }
        }
    }

    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        capture_event(data_mut, "on_ready", None, &format!("shard_{}", ready.shard.unwrap()[0].to_string())).await;

        info!("Shard {} connected as {}, on {} servers!", ready.shard.unwrap()[0], ready.user.name, ready.guilds.len());

        if !Path::new("/etc/probes").is_dir() {
            fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
        }
        if !Path::new("/etc/probes/live").exists() {
            let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
            file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
        }
    }

    /// Fires when the shard's status is updated
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        debug!("Shard stage changed");
        let alive = match event.new {
            serenity::gateway::ConnectionStage::Connected => true,
            _ => false,
        };

        if alive {
            if !Path::new("/etc/probes/live").exists() {
                fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
                let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
                file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
            }
        } else {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }

    /// Fires when a slash command or other interaction is received
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        if let Interaction::ApplicationCommand(command) = interaction.clone() {
            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : /{} {:?}", guild_id.name(&ctx.cache).unwrap(), guild_id.as_u64(), command.user.name, command.user.id.as_u64(), command.data.name, command.data.options);
                },
                None => {
                    info!("{:?} ({:?}) : /{} {:?}", command.user.name, command.user.id.as_u64(), command.data.name, command.data.options);
                }
            }
            let fake_embed = match command.data.name.as_str() {
                "ping" => {
                    FakeEmbed {
                        title: Some("Pong!".to_string()),
                        author: None,
                        description: None, // TODO - Add latency
                        url: None,
                        color: Some(Colour::from_rgb(0, 255, 0)),
                        footer: None,
                        image: None,
                        thumbnail: None,
                        timestamp: None,
                        fields: None,
                        buttons: None
                    }
                },

                "get" => {
                    let mut data = ctx.data.write().await;
                    get_subreddit_cmd(&command, &mut data, &ctx).await
                },

                "membership" => {
                    let mut data = ctx.data.write().await;
                    cmd_get_user_tiers(&command, &mut data, &ctx).await
                },

                "configure-server" => {
                    let mut data = ctx.data.write().await;
                    configure_server(&command, &mut data, &ctx).await
                },

                "custom" => {
                    get_custom_subreddit(&command, &ctx).await
                },

                "info" => {
                    let mut data = ctx.data.write().await;
                    info(&command, &mut data, &ctx).await
                },

                _ => {
                    FakeEmbed {
                        title: Some("Unknown command".to_string()),
                        author: None,
                        description: None,
                        url: None,
                        color: Some(Colour::from_rgb(255, 0, 0)),
                        footer: None,
                        image: None,
                        thumbnail: None,
                        timestamp: None,
                        fields: None,
                        buttons: None
                    }
                }
            };

            if let Err(why) = command
                .create_interaction_response(&ctx.http, |response| {
                    let to_pass = fake_embed.clone();
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(fake_embed_to_message!(to_pass))
                })
                .await
            {
                if format!("{}", why) == "Interaction has already been acknowledged." {
                    command.edit_original_interaction_response(&ctx.http, |response| {
                        debug!("Already sent response, editing instead");
                        let to_pass = fake_embed.clone();
                        response.embed(fake_embed_to_embed!(to_pass));
                        let to_pass = fake_embed.clone();
                        response.components(fake_embed_to_buttons!(to_pass))
                    }).await.unwrap();
                } else {
                    warn!("Cannot respond to slash command: {}", why);
                }
            }

            if fake_embed.image.is_some() {
                let url = fake_embed.image.clone().unwrap();
                if (url.contains("imgur")  && url.contains(".gif")) || url.contains("redgifs") {
                    match command.channel_id.send_message(&ctx.http, |message| {
                        message.content(url);

                        if fake_embed.buttons.is_some() {
                            message.components(fake_embed_to_buttons!(fake_embed));
                        }
                        message
                    }).await {
                        Ok(_) => {}
                        Err(why) => {
                            warn!("Cannot send followup to slash command: {}", why);
                        }
                    }
                }
            }
        }

        if let Interaction::MessageComponent(command) = interaction {
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();

            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : Button {} {:?}", guild_id.name(&ctx.cache).unwrap(), guild_id.as_u64(), command.user.name, command.user.id.as_u64(), command.data.custom_id, command.data.values);
                },
                None => {
                    info!("{:?} ({:?}) : /{} {:?}", command.user.name, command.user.id.as_u64(), command.data.custom_id, command.data.values);
                }
            }
            let subreddit = command.data.custom_id.split(":").last().unwrap().to_string();
            capture_event(data_mut, "subreddit_cmd_button", Some(HashMap::from([("subreddit", subreddit.clone())])), &format!("user_{}", command.user.id.0.to_string())).await;
            
            let mut con = match data_mut.get_mut("redis_connection").unwrap() {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(0),
            }.unwrap();

            let fake_embed = get_subreddit(subreddit, &mut con, command.channel_id).await;
            if let Err(why) = command
                .create_interaction_response(&ctx.http, |response| {
                    let to_pass = fake_embed.clone();
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(fake_embed_to_message!(to_pass))
                })
                .await
            {
                if format!("{}", why) == "Interaction has already been acknowledged." {
                    command.edit_original_interaction_response(&ctx.http, |response| {
                        debug!("Already sent response, editing instead");
                        let to_pass = fake_embed.clone();
                        response.embed(fake_embed_to_embed!(to_pass));
                        let to_pass = fake_embed.clone();
                        response.components(fake_embed_to_buttons!(to_pass))
                    }).await.unwrap();
                } else {
                    warn!("Cannot respond to slash command: {}", why);
                }
            }

            if fake_embed.image.is_some() {
                let url = fake_embed.image.clone().unwrap();
                if (url.contains("imgur")  && url.contains(".gif")) || url.contains("redgifs") {
                    match command.channel_id.send_message(&ctx.http, |message| {
                        message.content(url);

                        if fake_embed.buttons.is_some() {
                            message.components(fake_embed_to_buttons!(fake_embed));
                        }
                        message
                    }).await {
                        Ok(_) => {}
                        Err(why) => {
                            warn!("Cannot send followup to slash command: {}", why);
                        }
                    }
                }
            }
        }
    }
}


async fn monitor_total_shards(shard_manager: Arc<Mutex<serenity::client::bridge::gateway::ShardManager>>, total_shards: u64) {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_tokio_connection().await.expect("Can't connect to redis");

    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");

    loop {
        let _ = sleep(Duration::from_secs(60)).await;

        let db_total_shards: redis::RedisResult<u64> = con.get(format!("total_shards_{}", get_namespace().await)).await;
        let db_total_shards: u64 = db_total_shards.expect("Failed to get or convert total_shards from Redis");

        let mut shard_manager = shard_manager.lock().await;
        if db_total_shards != total_shards {
            debug!("Total shards changed from {} to {}, restarting.", total_shards, db_total_shards);
            shard_manager.set_shards(shard_id, 1, db_total_shards).await;
            shard_manager.initialize().expect("Failed to initialize shard");
        }

        /*let shard = shard_manager.
        match shard.heartbeat().await {
            Ok(_) => {
                debug!("Heartbeat sent successfully");
            },
            Err(why) => {
                warn!("Heartbeat failed: {}\nMarking self for restart.", why);
                fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
            }
        }*/

    }
}

async fn get_namespace() -> String {
    let namespace= fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
    return namespace;
}

#[tokio::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");
    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");

    let redis_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = redis_client.get_connection().expect("Can't connect to redis");

    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Shard {}", shard_id));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("settings");

    let filter = doc! {"id": "subreddit_list".to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    let doc = cursor.try_next().await.unwrap().unwrap();

    let nsfw_subreddits: Vec<String> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap().to_string()).collect();

    let total_shards: redis::RedisResult<u64> = con.get(format!("total_shards_{}", get_namespace().await));
    let total_shards: u64 = total_shards.expect("Failed to get or convert total_shards");

    let shard_id_logger = shard_id.clone();
    env_logger::builder()
    .format(move |buf, record| {
        writeln!(buf, "{}: Shard {}:{}", record.level(), shard_id_logger,  record.args())
    })
    .init();


    debug!("Booting with {:?} total shards", total_shards);

    debug!("Printing debug");
    info!("Printing info");
    warn!("Printing warn");
    error!("Printing error");

    let mut client = serenity::Client::builder(token,  GatewayIntents::non_privileged())
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("shard_id".to_string(), ConfigValue::U64(shard_id as u64)),
        ("redis_connection".to_string(), ConfigValue::REDIS(con)),
        ("mongodb_connection".to_string(), ConfigValue::MONGODB(mongodb_client)),
        ("nsfw_subreddits".to_string(), ConfigValue::SubredditList(nsfw_subreddits)),
        ("posthog_client".to_string(), ConfigValue::PosthogClient(posthog_client)),
    ]);

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigStruct>(contents);
    }

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        debug!("Spawning shard monitor thread");
        monitor_total_shards(shard_manager, total_shards).await;
    });

    let thread = tokio::spawn(async move {
        debug!("Spawning client thread");
        client.start_shard(shard_id, total_shards as u64).await.expect("Failed to start shard");
    });

    // If client thread exits, shard has crashed, so mark self as unhealthy.
    match thread.await {
        Ok(_) => {},
        Err(_) => {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }
}