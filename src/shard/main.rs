use log::*;
use std::{fs, thread};
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::env;
use crossbeam_utils;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use serenity::model::prelude::{CurrentUser, RoleId, Sticker, User, VoiceState};
use serenity::builder::{CreateApplicationCommandPermissionsData, CreateApplicationCommandPermissionData, CreateEmbed};
use serenity::model::interactions::application_command::ApplicationCommandPermissionType;
use serenity::model::id::{ApplicationId, ChannelId, EmojiId, GuildId, IntegrationId, MessageId, StickerId};
use serenity::model::gateway::{GatewayIntents, Presence};
use serenity::{
    async_trait,
    model::{
        gateway::Ready,
        interactions::{
            application_command::{
                ApplicationCommand,
                ApplicationCommandOptionType,
                ApplicationCommandInteraction,
            },
            Interaction,
            InteractionResponseType,
        },
    },
    prelude::*,
};
use tokio_tungstenite;
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use futures::prelude::stream::{SplitSink, SplitStream};
use tungstenite::Message;
use futures::{TryFutureExt, SinkExt, StreamExt};
use std::fmt::Error;
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::{RwLockWriteGuard, Arc};
use futures::executor::block_on;
use std::fs::File;
use std::panic::catch_unwind;
use serenity::http::Http;

use kube;
use kube::ResourceExt;
use k8s_openapi;
use kube::api::ListParams;

use redis;
use redis::{Commands, from_redis_value, RedisResult};
use serenity::client::bridge::gateway::event::ShardStageUpdateEvent;
use serenity::json::Value;
use serenity::model::application::command::CommandPermission;
use serenity::model::channel::{Channel, ChannelCategory, GuildChannel, PartialGuildChannel, Reaction, StageInstance};
use serenity::model::event::{ChannelPinsUpdateEvent, GuildMembersChunkEvent, GuildMemberUpdateEvent, GuildScheduledEventUserAddEvent, GuildScheduledEventUserRemoveEvent, InviteCreateEvent, InviteDeleteEvent, MessageUpdateEvent, ThreadListSyncEvent, ThreadMembersUpdateEvent, TypingStartEvent, VoiceServerUpdateEvent};
use serenity::model::guild::automod::{ActionExecution, Rule};
use serenity::model::guild::{Emoji, Guild, Integration, Member, PartialGuild, Role, ScheduledEvent, ThreadMember, UnavailableGuild};
use serenity::utils::Colour;

/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    RoleId(RoleId),
    Bool(bool),
    DB(redis::Connection)
}

/// Stores config values required for operation of the downloader
pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
}

impl TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
}


/// Represents a Reddit post
#[derive(Debug)]
pub struct Post {
    /// The score (upvotes-downvotes) of the post
    score: i64,
    /// A URl to the post
    url: String,
    title: String,
    /// Embed URL of the post media
    embed_url: String,
    /// Name of the author of the post
    author: String,
    /// The post's unique ID
    id: String,
}

#[derive(Debug)]
pub struct FakeEmbed {
    title: String,
    description: String,
    url: String,
    color: Colour,
    footer: String,
    image: String,
    thumbnail: String,
}


fn get_redis_con() -> redis::Connection {
    let db_client = redis::Client::open("redis://redis/").unwrap();
    return db_client.get_connection().expect("Can't connect to redis");
}


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn unbox<T>(value: Box<T>) -> T {
    *value
}

/// Checks if a user is a bot administrator
///
/// # Arguments
/// ## user
/// A [User](serenity::model::user::User) to check
async fn check_admin(user: serenity::model::user::User) -> bool { // Check if a user ID matches the admin user ID
    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
    .expect("config.json is not proper JSON");

    let admin = config.get("admin").unwrap().as_u64().unwrap();

    return admin == *user.id.as_u64();
}

async fn get_subreddit(command: &ApplicationCommandInteraction, ctx: &Context) -> FakeEmbed {
    let mut con = get_redis_con();

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.unwrap();
    let subreddit = subreddit.as_str().unwrap().to_string();

    let mut index: u16 = con.incr(format!("subreddit:{}:channels:{}:index", &subreddit, command.channel_id), 1).unwrap();
    index -= 1;
    let length: u16 = con.llen(format!("subreddit:{}:posts", &subreddit)).unwrap();
    index = length - index - 1;

    if index >= length {
        let _:() = con.set(format!("subreddit:{}:channels:{}:index", &subreddit, command.channel_id), 0).unwrap();
        index = 0;
    }

    let _:() = con.expire(format!("subreddit:{}:channels:{}:index", &subreddit, command.channel_id), 5*60).unwrap();

    let post: Vec<String> = redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(index).arg(index).query(&mut con).unwrap();
    let post: HashMap<String, redis::Value> = con.hgetall(&post[0]).unwrap();

    let embed = FakeEmbed {
        title: from_redis_value(&post.get("title").unwrap().clone()).unwrap(),
        description: from_redis_value(&post.get("author").unwrap().clone()).unwrap(),
        url: from_redis_value(&post.get("url").unwrap().clone()).unwrap(),
        color: Colour::from_rgb(0, 255, 0),
        footer: "".to_string(),
        image: from_redis_value(&post.get("embed_url").unwrap().clone()).unwrap(),
        thumbnail: "".to_string(),
    };

    return embed;
}

/// Discord event handler
struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("Shard {} connected as {}, on {} servers!", ready.shard.unwrap()[0], ready.user.name, ready.guilds.len());
        let guilds = ctx.cache.guild_count();
        let users = ctx.cache.user_count();

        fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
        let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
        file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
    }

    /// Fires when the shard's status is updated
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        debug!("Shard stage changed");
        let alive = match event.new {
            serenity::gateway::ConnectionStage::Connected => true,
            _ => false,
        };

        if alive {
            fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
            let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
            file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
        } else {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }

    /// Fires when a slash command or other interaction is received
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        if let Interaction::ApplicationCommand(command) = interaction {
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
                        title: "Pong!".to_string(),
                        description: "".to_string(), // TODO - Add latency
                        url: "".to_string(),
                        color: Colour::from_rgb(0, 255, 0),
                        footer: "".to_string(),
                        image: "".to_string(),
                        thumbnail: "".to_string(),
                    }
                },

                "get" => {
                    get_subreddit(&command, &ctx).await
                },

                _ => {
                    FakeEmbed {
                        title: "Unknown command".to_string(),
                        description: "".to_string(),
                        url: "".to_string(),
                        color: Colour::from_rgb(255, 0, 0),
                        footer: "".to_string(),
                        image: "".to_string(),
                        thumbnail: "".to_string(),
                    }
                }
            };

            debug!("{:?}", fake_embed);

            if let Err(why) = command
                .create_interaction_response(&ctx.http, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|message| message.embed(|e: &mut CreateEmbed| {
                            e.title(fake_embed.title.clone())
                                .description(fake_embed.description.clone())
                                .url(fake_embed.url.clone())
                                .color(fake_embed.color.clone())
                                .image(fake_embed.image.clone())
                                .thumbnail(fake_embed.thumbnail.clone())

            }))
                })
                .await
            {
                warn!("Cannot respond to slash command: {}", why);
            }
        }
    }
}


async fn monitor_total_shards(shard_manager: Arc<Mutex<serenity::client::bridge::gateway::ShardManager>>, mut total_shards: u64) {
    let db_client = redis::Client::open("redis://redis/").unwrap();
    let mut con = db_client.get_connection().expect("Can't connect to redis");

    loop {
        let _ = sleep(Duration::from_secs(60));

        let db_total_shards: redis::RedisResult<u64> = con.get("total_shards");
        let db_total_shards: u64 = db_total_shards.expect("Failed to get or convert total_shards");

        if db_total_shards != total_shards {
            debug!("Total shards changed from {} to {}, re-identifying with Discord.", total_shards, db_total_shards);

            let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
            let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");

            total_shards = db_total_shards;

            let mut shard_manager = shard_manager.lock().await;
            shard_manager.set_shards(shard_id, 1, total_shards).await;
            shard_manager.initialize().expect("Failed to initialise new shard manager");
        }
    }
}


#[tokio::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");

    let db_client = redis::Client::open("redis://redis/").unwrap();
    let mut con = db_client.get_connection().expect("Can't connect to redis");

    let total_shards: redis::RedisResult<u64> = con.get("total_shards");
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

    let mut client = Client::builder(token,  GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS)
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("shard_id".to_string(), ConfigValue::U64(shard_id as u64)),
        ("db_connection".to_string(), ConfigValue::DB(con)),
    ]);

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigStruct>(contents);
    }


    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
       monitor_total_shards(shard_manager, total_shards).await;
    });

    let thread = tokio::spawn(async move {
       client.start_shard(shard_id, total_shards as u64).await;
    });

    match thread.await {
        Ok(_) => {},
        Err(_) => {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }
}