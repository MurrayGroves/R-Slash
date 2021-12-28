use log::*;
use std::{fs, thread};
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use crossbeam_utils;
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
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use futures::prelude::stream::{SplitSink, SplitStream};
use tungstenite::Message;
use futures::{TryFutureExt, SinkExt, StreamExt};
use std::fmt::Error;
use std::time::{SystemTime, UNIX_EPOCH, Duration};
use std::sync::RwLockWriteGuard;
use futures::executor::block_on;

pub enum ConfigValue {
    U64(u64),
    RoleId(RoleId),
    Bool(bool),
    websocket_write(SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>),
    websocket_read(SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>)
}

pub struct ConfigStruct {
    _value: HashMap<String, ConfigValue>
}

impl TypeMapKey for ConfigStruct {
    type Value = HashMap<String, ConfigValue>;
}

struct Handler;

fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn check_admin(user: serenity::model::user::User) -> bool { // Check if a user ID matches the admin user ID
    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
    .expect("config.json is not proper JSON");

    let admin = config.get("admin").unwrap().as_u64().unwrap();

    return admin == user.id.as_u64();
}

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

async fn heartbeat_loop(mut data: tokio::sync::RwLockWriteGuard<'_, TypeMap>, initial_heartbeat: u64) {
    let mut last_heartbeat = initial_heartbeat;
    loop {
        if last_heartbeat == 0 {
            warn!("NO HEARTBEAT");
            thread::sleep(Duration::from_millis(10000)); // Sleep until on_ready() has completed and first heartbeat is sent.
            continue;
        }
        let mut time_to_heartbeat = (last_heartbeat+57000) - get_epoch_ms();  // How long until we need to send a heartbeat
        if time_to_heartbeat < 1500 {  // If less than 1.5 seconds until need to send a heartbeat
            debug!("SENDING HEARTBEAT");
            last_heartbeat = send_heartbeat(Some(0), Some(0), Some(0), &mut data).await;
            time_to_heartbeat = 56000;
        }
        thread::sleep(Duration::from_millis(time_to_heartbeat)); // Sleep until we need to send a heartbeat
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("Shard {} connected as {}, on {} servers.", ready.shard.unwrap()[0], ready.user.name, ready.guilds.len());
        let guilds = ctx.cache.guild_count().await;
        let users = ctx.cache.user_count().await;

        ApplicationCommand::set_global_application_commands(&ctx.http, |commands| {
            commands
                .create_application_command(|command| {
                    command.name("ping").description("A ping command")
                })
        })
        .await.expect("Failed to register slash commands");

        let mut data = ctx.data.write().await;
        let initial_heartbeat = send_heartbeat(Some(0), Some(guilds), Some(users), &mut data).await;

        heartbeat_loop(data, initial_heartbeat).await;
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        if let Interaction::ApplicationCommand(command) = interaction {
            let content = match command.data.name.as_str() {
                "ping" => "Hey, I'm alive!".to_string(),
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

#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    let coordinator = tokio_tungstenite::connect_async("ws://127.0.0.1:9002").await.expect("Failed to connect to coordinator");
    let response = coordinator.1.into_body();
    let coordinator = coordinator.0;
    let mut streams = coordinator.split();
    let mut write = streams.0;
    let read = streams.1;

    let token = std::env::args().nth(3).expect("no token given");
    let application_id: u64 = std::env::args().nth(4).expect("no application_id given").parse().expect("Failed to convert application_id to u64");
    let shard_id: usize = std::env::args().nth(1).expect("no shard_id given").parse().expect("Failed to convert shard_id to usize");
    let total_shards: usize = std::env::args().nth(2).expect("no total_shards given").parse().expect("Failed to convert total_shards to usize");

    let mut write = ConfigValue::websocket_write(write);

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("coordinator_write".to_string(), write),
        ("coordinator_read".to_string(), ConfigValue::websocket_read(read)),
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