use futures_util::{SinkExt, StreamExt};
use log::*;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, tungstenite::Error, WebSocketStream};
use tungstenite::Result;
use tungstenite::Message;
use std::{thread, fs, env, io::Write, collections::HashMap, process::Command, net::SocketAddr, time::Duration};
use std::sync::{Arc, Mutex};


#[derive(Debug)]
struct Shard {
    shard_id: usize,
    total_shards: usize, // Each shard can have a different value for total_shards that affects how much traffic is routed to them.
    guild_count: usize,
    user_count: usize, // This value includes users that are also in other shards.
    health: u8, // The health value from the shard's last heartbeat.
    last_heartbeat: u64,  // The unix timestamp of when the shard last sent a heartbeat.
    alive: bool,  // If the shard is considered functioning.
    connection: Option<WebSocketStream<TcpStream>>,
    process: Option<std::process::Child>,
}

use std::time::{SystemTime, UNIX_EPOCH};

fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

async fn boot_shard(shard_id: usize, total_shards: usize, token: &str, application_id: &str) -> Result<std::process::Child> {  // Returns PID of shard
    info!("Booting shard: {}", shard_id);
    let shard_id = &format!("{}", shard_id);
    let total_shards = &format!("{}", total_shards);
    let mut command = Command::new("target/debug/shard") // TODO - Find a better way of doing this, won't always be in that relative location. Maybe an environment variable.
        .args([shard_id, total_shards, token, application_id])
        .spawn();

    let process = command.expect("Failed to boot shard");
    info!("Shard {} has PID {}", shard_id, process.id());
    return Ok(process);
}

async fn reboot_shard(shard_id: usize, total_shards: usize, mut process: &mut std::process::Child) -> Result<std::process::Child> {  // Returns PID of shard
    info!("Killing shard {} with PID {:?}", shard_id, process.id());
    process.kill();
    process.wait();

    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
        .expect("config.json is not proper JSON");

    let token = config.get("token").unwrap().as_str().unwrap();
    let application_id = config.get("application_id").unwrap().as_str().unwrap();

    let process = boot_shard(shard_id, total_shards, token, application_id).await; // Boot new shard

    return process;
}

async fn check_for_missed_heartbeats(shards: Arc<Mutex<Vec<Shard>>>) {
    loop {
        thread::sleep(Duration::from_millis(2000)); // Check for missed heartbeats every 2 seconds
        let length = shards.lock().unwrap().len();
        for shard in 0..length {
            if shards.lock().unwrap()[shard].last_heartbeat < get_epoch_ms()-120000 { // If last heartbeat was greater than 2 heartbeat intervals ago
                shards.lock().unwrap()[shard].alive = false;
                shards.lock().unwrap()[shard].health = 0;
                warn!("Shard {} missed two heartbeats!", shards.lock().unwrap()[shard].shard_id);
                let shard_id = shards.lock().unwrap()[shard].shard_id;
                let total_shards = shards.lock().unwrap()[shard].total_shards;
                let mut process = shards.lock().unwrap()[shard].process.take().expect("No process");
                shards.lock().unwrap()[shard].process = Some(reboot_shard(shard_id, total_shards, &mut process).await.expect("No process returned by reboot_shard"));
            }
        }
    }
}

async fn heartbeat_request(request: HashMap<String, serde_json::Value>, shards: Arc<Mutex<Vec<Shard>>>) -> Result<String> { // Update the shard's status from a heartbeat
    let mut shards_lock = shards.lock().unwrap();
    for shard in 0..shards_lock.len() {
        if shards_lock[shard].shard_id == serde_json::from_value::<usize>(request["shard_id"].clone()).unwrap() {
            shards_lock[shard].last_heartbeat = get_epoch_ms();
            shards_lock[shard].health = serde_json::from_value::<u8>(request["health"].clone()).unwrap();
            shards_lock[shard].guild_count = serde_json::from_value::<usize>(request["guild_count"].clone()).unwrap();
            shards_lock[shard].user_count = serde_json::from_value::<usize>(request["user_count"].clone()).unwrap();
            shards_lock[shard].alive = true;
        }
    }

    debug!("{:?}", shards_lock);

    let output = "{\"type\": \"heartbeat_ack\"}";
    return Ok(output.to_string());
}

async fn accept_connection(peer: SocketAddr, stream: TcpStream, shards: Arc<Mutex<Vec<Shard>>>) {
    if let Err(e) = handle_connection(peer, stream, shards).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
            err => error!("Error processing connection: {:?}", err),
        }
    }
}

async fn handle_request(msg: &str, shards: Arc<Mutex<Vec<Shard>>>) -> Result<String> { // Take some request, process it, and provide an output.
    let request: HashMap<String, serde_json::Value> = serde_json::from_str(msg).unwrap();  // Convert config string into HashMap TODO - Error handling for non-json requests
    let request_type = serde_json::from_value::<String>(request["type"].clone()).unwrap();
    let output = match request_type.as_str() {
        "heartbeat" => heartbeat_request(request, shards).await?,
        _ => "Unknown Request".to_string(),
    };

    return Ok(output);
}

async fn handle_connection(peer: SocketAddr, stream: TcpStream, shards: Arc<Mutex<Vec<Shard>>>) -> Result<()> {
    let mut ws_stream = accept_async(stream).await.expect("Failed to accept");

    info!("New WebSocket connection: {}", peer);

    while let Some(msg) = ws_stream.next().await {
        let msg = msg?;
        if msg.is_text() || msg.is_binary() {
            let msg = msg.into_text().unwrap();
            debug!("Received request: {}", &msg);
            let output = handle_request(&msg, Arc::clone(&shards)).await?;  // TODO - Add error handling
            debug!("Response: {}", output);
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

    let mut shards:Arc<Mutex<Vec<Shard>>> = Arc::new(Mutex::new(Vec::new())); // Define a vector containing all the desired shards, even if offline.

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

    info!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);

    for x in 0..total_shards {
        let mut shard = Shard {
            shard_id: x,
            total_shards,
            guild_count: 0,
            user_count: 0,
            health: 0,
            last_heartbeat: 0,
            alive: false,
            connection: None,
            process: None,
        };

        shards.lock().unwrap().push(shard);
    }

    let addr = "127.0.0.1:9002";
    let listener = TcpListener::bind(&addr).await.expect("Can't listen");
    debug!("Coordinator listening on: {}", addr);
    {
        let mut shards_lock = shards.lock().unwrap();
        for shard in 0..shards_lock.len() {
            if shards_lock[shard].alive == false {
                shards_lock[shard].process = Some(boot_shard(shards_lock[shard].shard_id, shards_lock[shard].total_shards, token, application_id).await.unwrap());
            }
        }
    }

    loop {
        if let Ok((stream, _)) = listener.accept().await {
            let peer = stream.peer_addr().expect("connected streams should have a peer address");
            debug!("Peer address: {}", peer);

            tokio::spawn(check_for_missed_heartbeats(Arc::clone(&shards)));
            tokio::spawn(accept_connection(peer, stream, Arc::clone(&shards)));
        }
    }
}