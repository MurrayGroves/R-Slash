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

/// Represents a value stored in a [ConfigStruct](ConfigStruct)
pub enum ConfigValue {
    U64(u64),
    RoleId(RoleId),
    Bool(bool),
    websocket_write(SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>),
    websocket_read(SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>)
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

/// Send a heartbeat to the coordinator
///
/// Returns the current time since the Epoch
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
async fn send_heartbeat(health: Option<u8>, guild_count: Option<usize>, user_count: Option<usize>, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>) -> u64 {
    let shard_id = match data.get::<ConfigStruct>().unwrap().get("shard_id").unwrap() {
            ConfigValue::U64(x) => Ok(*x),
            _ => Err(0),
    }.unwrap();


    let mut websocket = match data.get_mut::<ConfigStruct>().unwrap().get_mut("coordinator_write").unwrap() {
        ConfigValue::websocket_write(x) => Ok(x),
        _ => Err(0)
    }.unwrap();



    let health:u8 = health.unwrap_or(0);
    let guild_count:usize = guild_count.unwrap_or(0);
    let user_count:usize = user_count.unwrap_or(0);

    let heartbeat = format!("{{\"type\": \"heartbeat\", \"shard_id\": {:?}, \"health\": {:?}, \"guild_count\": {:?}, \"user_count\": {:?}}}", shard_id, health, guild_count, user_count);

    websocket.send(tungstenite::Message::from(heartbeat.as_bytes())).await;
    return get_epoch_ms();
}

/// Starts the loop that sends heartbeats
///
/// # Arguments
/// ## data
/// A thread-safe wrapper of the [Config](ConfigStruct)
/// ## initial_heartbeat
/// The time at which the initial heartbeat was made on startup
async fn heartbeat_loop(mut data: tokio::sync::RwLockWriteGuard<'_, TypeMap>, initial_heartbeat: u64) {
    let mut last_heartbeat = initial_heartbeat;
    /* loop {
        if last_heartbeat == 0 {
            warn!("NO HEARTBEAT");
            sleep(Duration::from_millis(10000)).await; // Sleep until on_ready() has completed and first heartbeat is sent.
            continue;
        }
        let mut time_to_heartbeat:i64= ((last_heartbeat + 57000) as i64 - (get_epoch_ms() as i64));  // How long until we need to send a heartbeat
        if time_to_heartbeat < 1500 {  // If less than 1.5 seconds until need to send a heartbeat
            debug!("SENDING HEARTBEAT");
            last_heartbeat = send_heartbeat(Some(0), Some(0), Some(0), &mut data).await;
            time_to_heartbeat = 56000;
        }
        sleep(Duration::from_millis(time_to_heartbeat as u64)).await; // Sleep until we need to send a heartbeat
    } */
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
        info!("Shard {} connected as {}, on {} servers.", ready.shard.unwrap()[0], ready.user.name, ready.guilds.len());
        let guilds = ctx.cache.guild_count().await;
        let users = ctx.cache.user_count().await;

        if ready.shard.unwrap()[0] == 0 { // If shard 0, update slash commands
            update_slash_commands(&ctx.http).await;
        }

        let mut data = ctx.data.write().await;
        let initial_heartbeat = send_heartbeat(Some(0), Some(guilds), Some(users), &mut data).await;

        heartbeat_loop(data, initial_heartbeat).await;
    }

    /// Fires when a slash command or other interaction is received
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        if let Interaction::ApplicationCommand(command) = interaction {
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

/// Run on startup
///
/// Reads config file, starts websocket server, gets shard info from Discord, starts loops and starts sub-programs.
#[tokio::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: usize = env::var("SHARD_ID").expect("SHARD_ID not set").parse().expect("Failed to convert shard_id to usize");
    let total_shards: usize = env::var("TOTAL_SHARDS").expect("TOTAL_SHARDS not set").parse().expect("Failed to convert total_shards to usize");

    let shard_id_logger = shard_id.clone();
    env_logger::builder()
    .format(move |buf, record| {
        writeln!(buf, "{}: Shard {}:{}", record.level(), shard_id_logger,  record.args())
    })
    .init();

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("last_heartbeat".to_string(), ConfigValue::U64(0)),
        ("shard_id".to_string(), ConfigValue::U64(shard_id as u64)),
    ]);

    let mut client = Client::builder(token)
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigStruct>(contents);
    }

    if let Err(why) = block_on(client.start_shard(shard_id as u64, total_shards as u64)) {
        error!("Client error: {:?}", why);
    }
}