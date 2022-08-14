use log::*;
use std::{fs, thread};
use std::io::Write;
use std::collections::HashMap;
use std::iter::FromIterator;
use std::process;
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
use quit;

async fn update_slash_commands(http: &Arc<Http>) {
    let path = env::args_os().nth(0).expect("No config path provided");
    let config = fs::read_to_string(&path).expect("Couldn't read config file");  // Read config file in
    let config: Vec<HashMap<String, String>> = serde_json::from_str(&config)  // Convert config string into Vec of Hashmaps
    .expect("config file is not proper JSON");

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
    fs::write(&path, &json).expect("Unable to write to config file");
}



struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("Updating slash commands");
        update_slash_commands(&ctx.http).await;
        quit::with_code(0);
    }
}


#[tokio::main]
#[quit::main]
async fn main() {
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: usize = 0;
    let total_shards: usize = env::var("TOTAL_SHARDS").expect("TOTAL_SHARDS not set").parse().expect("Failed to convert total_shards to usize");

    let shard_id_logger = shard_id.clone();
    env_logger::builder()
    .format(move |buf, record| {
        writeln!(buf, "{}: Shard {}:{}", record.level(), shard_id_logger,  record.args())
    })
    .init();

    let mut client = Client::builder(token, GatewayIntents::non_privileged() | GatewayIntents::GUILD_MEMBERS)
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    if let Err(why) = block_on(client.start_shard(shard_id as u64, total_shards as u64)) {
        error!("Client error: {:?}", why);
    }
}