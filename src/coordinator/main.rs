use futures_util::{SinkExt, StreamExt};
use log::*;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Error, WebSocketStream};
use tungstenite::Result;
use tungstenite::Message;
use std::io::Write;
use std::fs;
use std::collections::HashMap;
use std::env;
use std::process::Command;

struct Shard {
    shard_id: usize,
    total_shards: usize, // Each shard can have a different value for total_shards that affects how much traffic is routed to them.
    guild_count: usize,
    user_count: usize, // This value includes users that are also in other shards.
    health: u8, // The health value from the shard's last heartbeat.
    last_heartbeat: u128,  // The unix timestamp of when the shard last sent a heartbeat.
    alive: bool,  // If the shard is considered functioning.
    connection: Option<WebSocketStream<TcpStream>>,
    pid: Option<usize>,
}

use std::time::{SystemTime, UNIX_EPOCH};

fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn boot_shard(shard_id: usize, total_shards: usize, token: &str, application_id: &str) -> Result<usize> {  // Returns PID of shard
        warn!("{}", env::current_dir().unwrap().display());

    let shard_id = &format!("{}", shard_id);
    let total_shards = &format!("{}", total_shards);
    let mut command = Command::new("target/debug/shard") // TODO - Find a better way of doing this, won't always be in that relative location. Maybe an environment variable.
        .args([shard_id, total_shards, token, application_id])
        .spawn();

    return Ok(command.unwrap().id() as usize);
}

async fn heartbeat_request(request: HashMap<String, serde_json::Value>) -> Result<&'static str> {
    let output = "Placeholder";
    return Ok(output);
}

async fn accept_connection(peer: SocketAddr, stream: TcpStream) {
    if let Err(e) = handle_connection(peer, stream).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
            err => error!("Error processing connection: {:?}", err),
        }
    }
}

async fn handle_request(msg: &str) -> Result<&str> { // Take some request, process it, and provide an output.
    let request: HashMap<String, serde_json::Value> = serde_json::from_str(msg).unwrap();  // Convert config string into HashMap *needs error handling for non-json requests*
    let request_type = serde_json::from_value::<String>(request["type"].clone()).unwrap();
    let output = match request_type.as_str() {
        "heartbeat" => heartbeat_request(request).await?,
        _ => "Unknown Request",
    };

    return Ok(output);
}

async fn handle_connection(peer: SocketAddr, stream: TcpStream) -> Result<()> {
    let mut ws_stream = accept_async(stream).await.expect("Failed to accept");

    warn!("New WebSocket connection: {}", peer);

    while let Some(msg) = ws_stream.next().await {
        let msg = msg?;
        if msg.is_text() || msg.is_binary() {
            let msg = msg.into_text().unwrap();
            warn!("Received request: {}", &msg);
            let output = handle_request(&msg).await?;  // Need to add error handling - TODO
            warn!("Response: {}", output);
            ws_stream.send(Message::text(output)).await?;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    warn!("{}", env::current_dir().unwrap().display());

    let mut shards:Vec<Shard> = Vec::new(); // Define a vector containing all the desired shards, even if offline.

    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
    .expect("config.json is not proper JSON");

    let token = config.get("token").unwrap().as_str().unwrap();
    let application_id = config.get("application_id").unwrap().as_str().unwrap();

    let client = reqwest::Client::new();
    let res = client
        .get("https://discord.com/api/v8/gateway/bot")
        .header("Authorization", format!("Bot {}", token))
        .send()
        .await;

    let json:serde_json::Value = res.unwrap().json().await.unwrap();
    let total_shards:usize = serde_json::from_value::<usize>(json["shards"].clone()).expect("Gateway response not usize");
    let max_concurrency = &json["session_start_limit"]["max_concurrency"];

    warn!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);

    for x in 0..total_shards {
        let mut shard = Shard {
            shard_id: x,
            total_shards: total_shards,
            guild_count: 0,
            user_count: 0,
            health: 0,
            last_heartbeat: 0,
            alive: false,
            connection: None,
            pid: None,
        };

        shards.push(shard);
    }

    let addr = "127.0.0.1:9002";
    let listener = TcpListener::bind(&addr).await.expect("Can't listen");
    warn!("Coordinator listening on: {}", addr);

    for shard in &mut shards {
        if shard.alive != true {
            shard.pid = Some(boot_shard(shard.shard_id, shard.total_shards, token, application_id).await.unwrap());
        }
    }



    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr().expect("connected streams should have a peer address");
        warn!("Peer address: {}", peer);

        tokio::spawn(accept_connection(peer, stream));
    }
}