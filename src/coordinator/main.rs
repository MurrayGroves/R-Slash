use futures_util::{SinkExt, StreamExt};
use log::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{accept_async, tungstenite::Error, WebSocketStream};
use tungstenite::Result;
use tungstenite::Message;
use std::{thread, fs, env, io::Write, collections::HashMap, process::Command, net::SocketAddr};
use std::sync::{Arc, Mutex};

mod request_handlers;

use crate::request_handlers::shard_request_handlers;
use crate::request_handlers::downloader_request_handlers;

/// Represents a Shard
#[derive(Debug)]
struct Shard {
    shard_id: isize,
    /// Each shard can have a different value for total_shards that affects how much traffic is routed to them.
    total_shards: isize,
    guild_count: usize,
    /// This value includes users that are also in other shards.
    user_count: usize,
    /// The health value from the shard's last heartbeat.
    /// `0` means healthy.
    health: u8,
    /// The unix timestamp of when the shard last sent a heartbeat.
    last_heartbeat: u64,
    /// If the shard is considered functioning.
    alive: bool,
    connection: Option<WebSocketStream<TcpStream>>,
    process: Option<std::process::Child>,
}

use std::time::{SystemTime, UNIX_EPOCH};

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Returns a [process](std::process::Child) representing the downloader instance
///
/// Will panic if the downloader couldn't be booted.
async fn boot_downloader() -> Result<std::process::Child> {
    info!("Booting downloader");
    let mut command = Command::new("target/debug/downloader") // TODO - Find a better way of doing this, won't always be in that relative location. Maybe an environment variable.
        .spawn();

    let process = command.expect("Failed to boot downloader");
    info!("Downloader has PID {}", process.id());
    return Ok(process);
}

/// Returns a [process](std::process::Child) representing the shard instance
///
/// Will panic if shard couldn't be booted.
async fn boot_shard(shard_id: isize, total_shards: isize, token: &str, application_id: &str) -> Result<std::process::Child> {
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

/// Reboots the given downloader [process](std::process::Child)
///
/// Returns the new downloader [process](std::process::Child)
/// Will kill the downloader, wait for it to die, and then boot a new downloader.
async fn reboot_downloader(mut process: &mut std::process::Child) -> Result<std::process::Child> { // Returns process object of downloader
    info!("Killing downloader with PID {:?}", process.id());
    process.kill();
    process.wait(); // Wait for program to close

    let process = boot_downloader().await; // Boot new downloader
    return process;
}

/// Reboots the given shard [process](std::process::Child)
///
/// Returns the new shard [process](std::process::Child)
/// Will kill the shard, wait for it to die, and then boot a new shard.
async fn reboot_shard(shard_id: isize, total_shards: isize, mut process: &mut std::process::Child) -> Result<std::process::Child> {  // Returns process object of shard
    info!("Killing shard {} with PID {:?}", shard_id, process.id());
    process.kill();
    process.wait(); // Wait for program to close

    let config = fs::read_to_string("config.json").expect("Couldn't read config.json");  // Read config file in
    let config: HashMap<String, serde_json::Value> = serde_json::from_str(&config)  // Convert config string into HashMap
        .expect("config.json is not proper JSON");

    let token = config.get("token").unwrap().as_str().unwrap();
    let application_id = config.get("application_id").unwrap().as_str().unwrap();

    let process = boot_shard(shard_id, total_shards, token, application_id).await; // Boot new shard

    return process;
}

/// Starts the loop that checks for missed heartbeats
///
/// Checks for missed heartbeats every two seconds.
/// If the shard has missed two heartbeats (two minutes), will reboot it.
async fn check_for_missed_heartbeats(shards: Arc<Mutex<Vec<Shard>>>) {
    loop {
        sleep(Duration::from_millis(2000)).await; // Check for missed heartbeats every 2 seconds
        let length = shards.lock().unwrap().len();
        for shard in 0..length {
            if shards.lock().unwrap()[shard].last_heartbeat < get_epoch_ms()-120000 { // If last heartbeat was greater than 2 heartbeat intervals ago
                shards.lock().unwrap()[shard].alive = false;
                shards.lock().unwrap()[shard].health = 0;
                warn!("Shard {} missed two heartbeats!", shards.lock().unwrap()[shard].shard_id);
                let shard_id = shards.lock().unwrap()[shard].shard_id;
                let total_shards = shards.lock().unwrap()[shard].total_shards;
                let mut process = shards.lock().unwrap()[shard].process.take().expect("No process");

                if shard_id == -1 {
                    shards.lock().unwrap()[shard].process = Some(reboot_downloader(&mut process).await.expect("No process returned by reboot_downloader"));
                }
                else {
                    shards.lock().unwrap()[shard].process= Some(reboot_shard(shard_id, total_shards, &mut process).await.expect("No process returned by reboot_shard"));
                }
            }
        }
    }
}

/// Handles an incoming heartbeat request
///
/// Will update the shard's status and returns a heartbeat acknowledgement of form `{"type": "heartbeat_ack"}`
async fn heartbeat_request(request: HashMap<String, serde_json::Value>, shards: Arc<Mutex<Vec<Shard>>>) -> Result<String> { // Update the shard's status from a heartbeat
    let mut shards_lock = shards.lock().unwrap();
    for shard in 0..shards_lock.len() {
        if shards_lock[shard].shard_id == serde_json::from_value::<isize>(request["shard_id"].clone()).unwrap() {
            shards_lock[shard].last_heartbeat = get_epoch_ms();
            if shards_lock[shard].shard_id != -1 {
                shards_lock[shard].health = serde_json::from_value::<u8>(request["health"].clone()).unwrap();
                shards_lock[shard].guild_count = serde_json::from_value::<usize>(request["guild_count"].clone()).unwrap();
                shards_lock[shard].user_count = serde_json::from_value::<usize>(request["user_count"].clone()).unwrap();
            }
            shards_lock[shard].alive = true;
        }
    }

    debug!("{:?}", shards_lock);

    let output = "{\"type\": \"heartbeat_ack\"}";
    return Ok(output.to_string());
}

/// Accepts an incoming websocket connection and passes it to [handle_connection](handle_connection)
async fn accept_connection(peer: SocketAddr, stream: TcpStream, shards: Arc<Mutex<Vec<Shard>>>) {
    if let Err(e) = handle_connection(peer, stream, shards).await {
        match e {
            Error::ConnectionClosed | Error::Protocol(_) | Error::Utf8 => (),
            err => error!("Error processing connection: {:?}", err),
        }
    }
}

/// Handles incoming websocket requests
///
/// Returns the String to respond with.
/// Responds with `Unknown Request` if request type is unknown.
/// Calls the appropriate function and returns the corresponding response.
async fn handle_request(msg: &str, shards: Arc<Mutex<Vec<Shard>>>) -> Result<String> { // Take some request, process it, and provide an output.
    let request: HashMap<String, serde_json::Value> = serde_json::from_str(msg).unwrap();  // Convert config string into HashMap TODO - Error handling for non-json requests
    let request_type = serde_json::from_value::<String>(request["type"].clone()).unwrap();
    let output = match request_type.as_str() {
        "heartbeat" => heartbeat_request(request, shards).await?,
        "get_subreddit_list" => downloader_request_handlers::get_subreddit_list(),
        _ => "Unknown Request".to_string(),
    };

    return Ok(output);
}

/// Handles a websocket connection
///
/// Receives incoming messages and responds with the output of [handle_request](handle_request)
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

/// Run on startup
///
/// Reads config file, starts websocket server, gets shard info from Discord, starts loops and starts sub-programs.
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
    let total_shards:isize = serde_json::from_value::<isize>(json["shards"].clone()).expect("Gateway response not usize");
    let max_concurrency = &json["session_start_limit"]["max_concurrency"];

    info!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);

    for x in -1..total_shards {
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
            if shards_lock[shard].shard_id == -1 && shards_lock[shard].alive == false {
                shards_lock[shard].process = Some(boot_downloader().await.unwrap());
            }
            if shards_lock[shard].alive == false && shards_lock[shard].shard_id > -1 {
                shards_lock[shard].process = Some(boot_shard(shards_lock[shard].shard_id, shards_lock[shard].total_shards, token, application_id).await.unwrap());
            }
        }
    }

    tokio::spawn(check_for_missed_heartbeats(Arc::clone(&shards)));

    loop {
        if let Ok((stream, _)) = listener.accept().await {
            let peer = stream.peer_addr().expect("connected streams should have a peer address");
            debug!("Peer address: {}", peer);

            tokio::spawn(accept_connection(peer, stream, Arc::clone(&shards)));
        }
    }
}