use k8s_openapi;
use kube::api::Patch;
use log::*;
use redis::{from_redis_value, Commands, Value};
use std::cmp;
use std::convert::TryInto;
use std::{env, fs, io::Write, process::Command, thread};
use tokio::time::{sleep, Duration};

async fn get_namespace() -> String {
	let namespace = fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
		.expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
	return namespace;
}

async fn add_shards(num: u64, max_concurrency: u64) {
	let namespace = get_namespace().await;

	let mut desired_shards = num;
	loop {
		if desired_shards == 0 {
			info!("All shards started");
			break;
		}

		let new_shards = cmp::min(desired_shards, max_concurrency);

		desired_shards -= new_shards;

		let client_k8s = kube::Client::try_default().await.unwrap();

		let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
			kube::Api::namespaced(client_k8s, &namespace);
		let shards_set = stateful_sets
			.get("discord-shards")
			.await
			.expect("Failed to get statefulset discord-shards");
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

		info!(
            "Booting {} new shards, bringing total to {}",
            new_shards,
            new_shards + current_shards
        );

		let mut params = kube::api::PatchParams::apply("rslash-manager");
		params.force = true;
		let patch = Patch::Apply(&patch);
		let _ = stateful_sets
			.patch("discord-shards", &params, &patch)
			.await
			.expect("Failed to patch statefulset discord-shards");

		loop {
			let client_k8s = kube::Client::try_default().await.unwrap();
			let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
				kube::Api::namespaced(client_k8s, &namespace);
			let shards_set = stateful_sets
				.get("discord-shards")
				.await
				.expect("Failed to get statefulset discord-shards");
			let ready_shards: u64 = shards_set
				.status
				.unwrap()
				.available_replicas
				.unwrap()
				.try_into()
				.unwrap();
			if ready_shards == new_shards + current_shards {
				break;
			}

			let _ = sleep(Duration::from_secs(1));
		}
	}
}

async fn get_redis_connection() -> redis::Connection {
	let db_client =
		redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap();
	let con = db_client.get_connection().expect("Can't connect to redis");
	return con;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	env_logger::builder()
		.format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
		.init();

	let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

	let namespace = get_namespace().await;

	let client_web = reqwest::Client::new();

	let mut con = get_redis_connection().await;
	loop {
		let client_k8s = kube::Client::try_default().await?;
		let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
			kube::Api::namespaced(client_k8s, &namespace);
		let shards_set = stateful_sets
			.get("discord-shards")
			.await
			.expect("Failed to get statefulset discord-shards");
		let current_shards = shards_set.status.unwrap().replicas as u64;

		let res = client_web
			.get("https://discord.com/api/v8/gateway/bot")
			.header("Authorization", format!("Bot {}", token))
			.send()
			.await;

		let json: serde_json::Value = res.unwrap().json().await.unwrap();
		let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone())
			.expect("Gateway response not u64");

		let max_concurrency: u64 = *&json["session_start_limit"]["max_concurrency"]
			.as_u64()
			.unwrap();

		debug!(
            "Gateway wants {} shards, {} at a time",
            &total_shards, &max_concurrency
        );
		if &total_shards > &current_shards {
			info!(
                "Gateway wants {:?} shards, but we only have {:?}",
                &total_shards, &current_shards
            );

			let manual_sharding: Value = con.get("manual_sharding").unwrap();
			let manual_sharding: String =
				from_redis_value(&manual_sharding).unwrap_or("false".to_string());
			let manual_sharding: bool = manual_sharding.parse::<bool>().unwrap();
			if manual_sharding {
				info!("Manual sharding enabled, doing nothing.");
				thread::sleep(Duration::from_secs(60 * 15));
				continue;
			}
			info!("Booting new shards");
			let _: () = con
				.set(format!("total_shards_{}", namespace), total_shards)
				.expect("Failed to set total shards");
			add_shards(total_shards - current_shards, max_concurrency).await;
		}
		thread::sleep(Duration::from_secs(60 * 15));
	}
}
