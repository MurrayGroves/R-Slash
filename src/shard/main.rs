use log::*;
use std::{fs, thread};
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::env;
use crossbeam_utils;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use serenity::model::prelude::RoleId;
use serenity::builder::{CreateApplicationCommandPermissionsData, CreateApplicationCommandPermissionData};
use serenity::model::interactions::application_command::ApplicationCommandPermissionType;
use serenity::model::id::{GuildId};
use serenity::model::gateway::GatewayIntents;
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
use serenity::http::Http;

use kube;
use kube::ResourceExt;
use k8s_openapi;
use kube::api::ListParams;

use redis;
use redis::Commands;

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


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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


/// Check slash commands config for changes, and update
async fn update_slash_commands(http: &Arc<Http>) {
    let config = fs::read_to_string("/etc/config/slash-commands.json").expect("Couldn't read slash_commands.json");  // Read config file in
    let config: Vec<HashMap<String, String>> = serde_json::from_str(&config)  // Convert config string into Vec of Hashmaps
    .expect("slash_commands.json is not proper JSON");

    let mut new_config:Vec<HashMap<String, String>> = Vec::new();
    for cur_command in config {
        let empty = String::new();
        let mut to_hash = String::new();
        to_hash.push_str(cur_command.get("name").unwrap_or(&empty));
        to_hash.push_str(cur_command.get("description").unwrap_or(&empty));
        let old_hash:u64 = match cur_command.get("hash") {
            Some(x) => x.parse().expect("Failed to parse hash"),
            None => 0,
        };
        let mut hasher =  DefaultHasher::new();
        to_hash.hash(&mut hasher);
        let to_hash = hasher.finish();

        if &to_hash != &old_hash { // If changes have been made
            ApplicationCommand::create_global_application_command(http, |command| {
                command
                    .name(cur_command.get("name").unwrap_or(&"".to_string()))
                    .description(cur_command.get("description").unwrap_or(&"".to_string()))
            }).await;
            info!("Creating new command: {}", cur_command.get("name").unwrap_or(&"".to_string()));
        }

        let mut new_command: HashMap<String, String> = cur_command.clone();
        new_command.insert("hash".to_string(), to_hash.to_string());
        new_config.push(new_command);

    }
    let json:String = serde_json::to_string(&new_config).expect("Failed to convert new config to json");
    fs::write("slash_commands.json", &json).expect("Unable to write to slash_commands.json");
}

async fn get_subreddit(command: &ApplicationCommandInteraction, ctx: &Context) -> String {
    return "passed".to_string();
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
            let content = match command.data.name.as_str() {
                "ping" => "Hey, I'm alive!".to_string(),
                "get_subreddit" => get_subreddit(&command, &ctx).await,
                _ => "Encountered error, please report in support server: `interaction response not found`".to_string(),
            };

            if let Err(why) = command
                .create_interaction_response(&ctx.http, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|message| message.content(content))
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

    if let Err(why) = block_on(client.start_shard(shard_id, total_shards as u64)) {
        error!("Client error: {:?}", why);
    }
}