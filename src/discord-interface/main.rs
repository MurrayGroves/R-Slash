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
use k8s_openapi::api::core::v1::Pod;

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

    let pods: Api<Pod> = Api::namespaced(client_k8s, "r-slash");
    for p in pods.list(&ListParams::default()).await? {
        println!("found pod {}", p.name());
    }

    loop {
        let res = client_web
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

        let json: serde_json::Value = res.unwrap().json().await.unwrap();
        let total_shards: isize = serde_json::from_value::<isize>(json["shards"].clone()).expect("Gateway response not usize");
        let max_concurrency = &json["session_start_limit"]["max_concurrency"];

        info!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);
        thread::sleep(Duration::from_secs(60*15));
    }
}