use std::env;
use std::process::Command;
use kube::api::PatchParams;
use serde_json;
use redis;
use redis::Commands;
use std::cmp;
use std::fs::read;
use std::thread::current;
use std::convert::TryInto;
use tokio::time::sleep;
use std::time::Duration;
use kube::api::Patch;
use base64;

/*let client_k8s = kube::Client::try_default().await.expect("Failed to connect to k8s");

let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
let total_shards = shards_set.metadata.annotations.expect("");
let total_shards = total_shards.get("kubectl.kubernetes.io/last-applied-configuration").unwrap();
let total_shards: serde_json::Value = serde_json::from_str(total_shards).unwrap();
let total_shards: u64 = total_shards["spec"]["replicas"].as_u64().unwrap(); */

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

async fn get_shard_set() -> k8s_openapi::api::apps::v1::StatefulSet {
    let client_k8s = kube::Client::try_default().await.expect("Failed to connect to k8s");

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
    let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get discord-credentials");
    return shards_set;
}


async fn update_max_unavailable() {
    let (total_shards, max_concurrency) = get_discord_details().await;
    let client_k8s = kube::Client::try_default().await.unwrap();

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
    let patch = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {
            "name": "discord-shards",
            "namespace": "r-slash"
        },
        "spec": {
            "updateStrategy": {
                "type": "RollingUpdate",
                "rollingUpdate": {
                    "maxUnavailable": max_concurrency
                }
            }
        }
    });

    let mut params = kube::api::PatchParams::apply("rslash-manager");
    params.force = true;
    let patch = Patch::Apply(&patch);
    let _ = stateful_sets.patch("discord-shards", &params, &patch).await.expect("Failed to patch statefulset discord-shards");
}

async fn get_discord_details() -> (u64, u64) {
    let client_k8s = kube::Client::try_default().await.unwrap();
    let credentials: kube::Api<k8s_openapi::api::core::v1::Secret> = kube::Api::namespaced(client_k8s, "r-slash");
    let credentials = credentials.get("discord-credentials").await.unwrap();
    let credentials = credentials.data.unwrap();
    let token = credentials["DISCORD_TOKEN"].clone();
    let token = String::from_utf8(token.0).unwrap();

    let web_client = reqwest::Client::new();
    let res = web_client
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

    let json: serde_json::Value = res.unwrap().json().await.unwrap();
    let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone()).expect("Gateway response not u64");
    let max_concurrency: u64 = serde_json::from_value::<u64>(json["session_start_limit"]["max_concurrency"].clone()).expect("Gateway response not u64");

    return (total_shards, max_concurrency);
}


async fn stop() {
        let mut con = get_redis_connection().await;
        let _:() = con.set("manual_sharding", "true").expect("Failed to set manual_sharding"); // Tells discord-interface to release control of sharding

        let client_k8s = kube::Client::try_default().await.unwrap();

        let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
        let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");

        let patch = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": {
                "name": "discord-shards",
                "namespace": "r-slash"
            },
            "spec": {
                "replicas": 0
            }
        });

        println!("Setting replicas to 0.");

        let mut params = kube::api::PatchParams::apply("rslash-manager");
        params.force = true;
        let patch = Patch::Apply(&patch);
        let _ = stateful_sets.patch("discord-shards", &params, &patch).await.expect("Failed to patch statefulset discord-shards");
}


async fn start() {
    let (gateway_total_shards, max_concurrency) = get_discord_details().await;

    let total_shards = match {env::args_os().nth(2)} { // Check if total_shards provided in command invocation
        Some(x) => x.into_string().unwrap().parse().unwrap(),
        None => gateway_total_shards
    };

    let mut con = get_redis_connection().await;
    let _:() = con.set("total_shards", total_shards).expect("Failed to set total_shards");
    let _:() = con.set("max_concurrency", max_concurrency).expect("Failed to set max_concurrency");
    let _:() = con.set("manual_sharding", "true").expect("Failed to set manual_sharding"); // Tells discord-interface to release control of sharding

    let mut desired_shards = total_shards;
    loop {
        if desired_shards == 0 {
            println!("All shards started");
            break;
        }

        let new_shards = cmp::min(desired_shards, max_concurrency);

        desired_shards -= new_shards;

        let client_k8s = kube::Client::try_default().await.unwrap();

        let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
        let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
        let current_shards = shards_set.metadata.annotations.expect("");
        let current_shards = current_shards.get("kubectl.kubernetes.io/last-applied-configuration").unwrap();
        let current_shards: serde_json::Value = serde_json::from_str(current_shards).unwrap();
        let current_shards: u64 = current_shards["spec"]["replicas"].as_u64().unwrap();

        let patch = serde_json::json!({
            "apiVersion": "apps/v1",
            "kind": "StatefulSet",
            "metadata": {
                "name": "discord-shards",
                "namespace": "r-slash"
            },
            "spec": {
                "replicas": new_shards + current_shards
            }
        });

        println!("Booting {} new shards, bringing total to {}", new_shards, new_shards + current_shards);

        let mut params = kube::api::PatchParams::apply("rslash-manager");
        params.force = true;
        let patch = Patch::Apply(&patch);
        let _ = stateful_sets.patch("discord-shards", &params, &patch).await.expect("Failed to patch statefulset discord-shards");

        loop {
            let client_k8s = kube::Client::try_default().await.unwrap();
            let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, "r-slash");
            let shards_set = stateful_sets.get("discord-shards").await.expect("Failed to get statefulset discord-shards");
            let ready_shards: u64 = shards_set.status.unwrap().available_replicas.unwrap().try_into().unwrap();
            if ready_shards == new_shards + current_shards {
                break;
            }

            let _ = sleep(Duration::from_secs(1));
        }
    }
    let _:() = con.set("manual_sharding", "false").expect("Failed to set manual_sharding"); // Tells discord-interface to take control of sharding again
}


async fn update() {
    update_max_unavailable().await;

    match {env::args_os().nth(2)} { // Check if a tag is provided
        Some(x) => {
            let tag: String = x.into_string().unwrap().parse().unwrap();
            let mut rollout = Command::new("kubectl");
            rollout.arg("set").arg("-n").arg("r-slash").arg("image").arg("statefulset/discord-shards").arg(format!("discord-shard=discord-shard:{}", tag));
            let output = rollout.output().expect("Failed to run kubectl");
            println!("{:?}", output.stdout);
            println!("{:?}", output.stderr);
        },
        None => {
            let mut rollout = Command::new("kubectl");
            rollout.arg("rollout").arg("-n").arg("r-slash").arg("restart").arg("statefulset/discord-shards");
            let output = rollout.output().expect("Failed to run kubectl");
            println!("{:?}", output.stdout);
            println!("{:?}", output.stderr);
        }
    };
}


#[tokio::main]
async fn main() {
    match env::args_os().nth(1).expect("No command provided").to_str().unwrap() {
        "start" => start().await,
        "stop" => stop().await,
        "update" => update().await,
        "restart" => update().await, // Calling update without a tag just restarts all shards anyway
        _ => println!("No Command Provided"),
    }
}