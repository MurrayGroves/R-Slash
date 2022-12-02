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
use redis::{Commands, from_redis_value, Value};
use std::cmp;
use kube::api::Patch;
use std::convert::TryInto;

async fn get_namespace() -> String {
    let namespace= fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
    return namespace;
}

async fn add_shards(num: u64, max_concurrency: u64) {
    let mut con = get_redis_connection().await;

    let client_k8s = kube::Client::try_default().await.unwrap();

    let namespace = get_namespace().await;

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
    let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
    let current_shards = shards_set.metadata.annotations.expect("");
    let current_shards: u64 = shards_set.status.unwrap().replicas.try_into().unwrap();


    let mut desired_shards = num;
    loop {
        if desired_shards == 0 {
            info!("All shards started");
            break;
        }

        let new_shards = cmp::min(desired_shards, max_concurrency);

        desired_shards -= new_shards;

        let client_k8s = kube::Client::try_default().await.unwrap();

        let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
        let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
        let current_shards = shards_set.metadata.annotations.expect("");
        let current_shards: u64 = shards_set.status.unwrap().replicas.try_into().unwrap();

        let patch = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": {
                "name": "discord-shards",
                "namespace": namespace
            },
            "spec": {
                "replicas": new_shards + current_shards
            }
        });

        info!("Booting {} new shards, bringing total to {}", new_shards, new_shards + current_shards);
        let _:() = con.set(format!("total_shards_{}", namespace), new_shards + current_shards).expect("Failed to set total shards");

        let mut params = kube::api::PatchParams::apply("rslash-manager");
        params.force = true;
        let patch = Patch::Apply(&patch);
        let _ = stateful_sets.patch("discord-shards", &params, &patch).await.expect("Failed to patch statefulset discord-shards");

        loop {
            let client_k8s = kube::Client::try_default().await.unwrap();
            let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
            let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
            let ready_shards: u64 = shards_set.status.unwrap().available_replicas.unwrap().try_into().unwrap();
            if ready_shards == new_shards + current_shards {
                break;
            }

            let _ = sleep(Duration::from_secs(1));
        }
    }
}

async fn get_redis_connection() -> redis::Connection {
    let mut get_ip = Command::new("kubectl");
    get_ip.arg("get").arg("nodes").arg("-o").arg("json");
    let output = get_ip.output().expect("Failed to run kubectl");
    let ip_output = String::from_utf8(output.stdout).expect("Failed to convert output to string");
    let ip_json: Result<serde_json::Value, serde_json::Error> = serde_json::from_str(&ip_output);
    let ip = ip_json.expect("Failed to convert string to json")["items"][0]["status"]["addresses"][0]["address"].to_string().replace('"', "");
    let db_client = redis::Client::open(format!("redis://{}:31090/", ip)).unwrap();
    let mut con = db_client.get_connection().expect("Can't connect to redis");
    return con;
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

    let namespace = get_namespace().await;

    let client_web = reqwest::Client::new();

    let mut con = get_redis_connection().await;
    loop {
        let client_k8s = kube::Client::try_default().await?;
        let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
        let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
        let current_shards = shards_set.status.unwrap().replicas as u64;
        //let current_shards = current_shards.get("kubectl.kubernetes.io/last-applied-configuration").unwrap();
        //let current_shards: serde_json::Value = serde_json::from_str(current_shards).unwrap();
        //let mut current_shards: u64 = current_shards["spec"]["replicas"].as_u64().unwrap();

        let res = client_web
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

        let json: serde_json::Value = res.unwrap().json().await.unwrap();
        let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone()).expect("Gateway response not u64");
        let max_concurrency: u64 = *&json["session_start_limit"]["max_concurrency"].as_u64().unwrap();

        debug!("Gateway wants {} shards, {} at a time", &total_shards, &max_concurrency);
        if &total_shards > &current_shards {
            info!("Gateway wants {:?} shards, but we only have {:?}", &total_shards, &current_shards);

            let manual_sharding: Value = con.get("manual_sharding").unwrap();
            let manual_sharding:String = from_redis_value(&manual_sharding).unwrap();
            let manual_sharding: bool = manual_sharding.parse::<bool>().unwrap();
            if manual_sharding {
                info!("Manual sharding enabled, doing nothing.");
                thread::sleep(Duration::from_secs(60*15));
                continue;
            }
            info!("Booting new shards");
            add_shards(total_shards-current_shards, max_concurrency).await;

        }
        thread::sleep(Duration::from_secs(60*15));
    }
}