use log::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{accept_async, tungstenite::Error, WebSocketStream};
use tungstenite::Result;
use tungstenite::Message;
use std::{thread, fs, env, io::Write, collections::HashMap, process::Command, net::SocketAddr};
use std::sync::{Arc, Mutex};
use futures::{StreamExt, TryStreamExt};
use kube::{Client, api::{Api, ResourceExt, ListParams, PostParams}};
use k8s_openapi;

async fn add_shards(num: u64, client_k8s: Client) {

}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set");

    let client_web = reqwest::Client::new();
    let client_k8s = kube::Client::try_default().await?;

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
    let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
    let current_shards = shards_set.metadata.annotations.expect("");
    let current_shards = current_shards.get("kubectl.kubernetes.io/last-applied-configuration").unwrap();
    let current_shards: serde_json::Value = serde_json::from_str(current_shards).unwrap();
    let mut current_shards: u64 = current_shards["spec"]["replicas"].as_u64().unwrap();

    let mut con = get_redis_connection().await;
    loop {
        let res = client_web
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

        let json: serde_json::Value = res.unwrap().json().await.unwrap();
        let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone()).expect("Gateway response not u64");
        let max_concurrency = &json["session_start_limit"]["max_concurrency"];

        debug!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);
        if &total_shards != &current_shards {
            info!("Gateway wants {:?} shards, but we only have {:?}", &total_shards, &current_shards);

            let manual_sharding: redis::RedisResult<bool> = con.get("manual_sharding");
            let manual_sharding: bool = manual_sharding.expect("Failed to convert manual_sharding to bool");
            if manual_sharding {
                info!("Manual sharding enabled, doing nothing.");
                continue;
            }

        }
        thread::sleep(Duration::from_secs(60*15));
    }
}