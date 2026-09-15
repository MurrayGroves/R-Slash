mod discord_interface;

use crate::discord_interface::DiscordInterface;
use futures::StreamExt;
use k8s_openapi;
use kube::api::Patch;
use log::*;
use metrics::counter;
use redis::aio::MultiplexedConnection;
use redis::{AsyncTypedCommands, Value, from_redis_value};
use rootcause::hooks::Hooks;
use rootcause::prelude::{Report, ResultExt};
use rootcause_backtrace::BacktraceCollector;
use std::convert::TryInto;
use std::ops::Index;
use std::sync::Arc;
use std::{cmp, future};
use std::{env, fs, io::Write, thread};
use tarpc::context::Context;
use tarpc::server;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

struct ServerState {
    max_concurrency: usize,
    current_bucket: Option<usize>,
    starting_shards: Vec<usize>,
}

#[derive(Clone)]
struct Server {
    state: Arc<Mutex<ServerState>>,
}

impl Server {
    async fn set_concurrency(&self, max_concurrency: usize) {
        let mut state = self.state.lock().await;
        state.max_concurrency = max_concurrency;
    }
}

impl DiscordInterface for Server {
    async fn request_boot(self, context: Context, shard_id: usize) -> bool {
        let mut state = self.state.lock().await;
        if state.max_concurrency == 0 {
            counter!("discordinterface_rejected_boot_requests").increment(1);
            return false;
        }
        let shard_bucket = shard_id / state.max_concurrency;

        let allowed = if let Some(bucket) = state.current_bucket {
            debug!(
                "Current bucket is {}-{}",
                state.max_concurrency * bucket,
                state.max_concurrency * (bucket + 1)
            );
            if bucket == shard_bucket {
                debug!("Shard {} in current bucket {}", shard_id, bucket);
                state.starting_shards.push(shard_id);
                true
            } else {
                false
            }
        } else {
            debug!("No bucket active, setting {}", shard_bucket);
            state.current_bucket = Some(shard_bucket);
            state.starting_shards.push(shard_id);
            true
        };

        let state = self.state.clone();
        if allowed {
            tokio::spawn(async move {
                sleep(Duration::from_secs(30)).await;
                debug!("Checking shard {} has finished", shard_id);
                let mut state = state.lock().await;
                let index = state.starting_shards.iter().position(|x| *x == shard_id);

                if let Some(index) = index {
                    warn!(
                        "Shard {} hadn't finished booting! Force removing...",
                        shard_id
                    );
                    state.starting_shards.remove(index);
                    if state.starting_shards.len() == 0 {
                        state.current_bucket = None;
                    }
                    counter!("discordinterface_timedout_boots").increment(1);
                } else {
                    debug!("Shard {} already finished booting, no problems", shard_id);
                    counter!("discordinterface_successful_boots").increment(1);
                }
            });
        };

        info!("Shard {} allowed: {}", shard_id, allowed);
        match allowed {
            true => counter!("discordinterface_allowed_boot_requests").increment(1),
            false => counter!("discordinterface_rejected_boot_requests").increment(1),
        }
        allowed
    }

    async fn finished_boot(self, context: Context, shard_id: usize) -> () {
        let mut state = self.state.lock().await;

        let index = state.starting_shards.iter().position(|x| *x == shard_id);

        if let Some(index) = index {
            debug!("Shard {} finished booting", shard_id);
            state.starting_shards.remove(index);
        } else {
            warn!("Shard {} finished booting but was not present", shard_id);
        }
        if state.starting_shards.len() == 0 {
            state.current_bucket = None;
        }
    }
}

async fn get_namespace() -> String {
    let namespace = fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
    return namespace;
}

async fn scale_down_shards(new_shard_count: u64) {
    let namespace = get_namespace().await;

    let client_k8s = kube::Client::try_default().await.unwrap();

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
        kube::Api::namespaced(client_k8s, &namespace);

    let patch = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {
            "name": "discord-shards",
            "namespace": namespace
        },
        "spec": {
            "replicas": new_shard_count,
        }
    });

    info!("Scaling shards down to {}", new_shard_count,);

    let mut params = kube::api::PatchParams::apply("rslash-manager");
    params.force = true;
    let patch = Patch::Apply(&patch);
    stateful_sets
        .patch("discord-shards", &params, &patch)
        .await
        .expect("Failed to patch statefulset discord-shards");
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

            sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn get_redis_connection() -> redis::aio::MultiplexedConnection {
    let db_client =
        redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap();
    db_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis")
}

async fn query_gateway(
    web_client: &reqwest::Client,
    token: &str,
) -> Result<serde_json::Value, Report> {
    loop {
        let res = web_client
            .get("https://discord.com/api/v10/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .context("Failed to make request to gateway")?;

        let headers = format!("{:?}", res.headers());

        let text = res
            .text()
            .await
            .context("Failed to parse gateway response as text")?;

        if text == "error code: 1015" {
            // Hit by Cloudflare rate limit
            warn!("Hit by Cloudflare rate limit, sleeping for 15 minutes");
            info!("Headers: {:?}", headers);
            sleep(Duration::from_mins(15)).await;
        } else {
            return Ok(serde_json::from_str(&text)
                .context("Failed to parse gateway response as JSON")
                .attach(text)?);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Report> {
    env_logger::builder()
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .install()
        .expect("Failed to install Prometheus exporter");

    Hooks::new()
        .report_creation_hook(BacktraceCollector::new_from_env())
        .install()
        .expect("failed to install backtrace hooks");

    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

    let namespace = get_namespace().await;

    let client_web = reqwest::Client::new();

    let server = Server {
        state: Arc::new(Mutex::new(ServerState {
            max_concurrency: 0,
            current_bucket: None,
            starting_shards: Vec::new(),
        })),
    };

    let mut listener = tarpc::serde_transport::tcp::listen("0.0.0.0:50051", Bincode::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);

    // Spawn tarpc server in background
    let server_clone = server.clone();
    tokio::spawn(
        listener
            // Ignore accept errors.
            .filter_map(|r| future::ready(r.ok()))
            .map(server::BaseChannel::with_defaults)
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated SubscriberServer trait.
            .map(move |channel| {
                let server = server_clone.clone();
                channel
                    .execute(server.serve())
                    .for_each(|response| async move {
                        spawn(response);
                    })
            })
            // Max 10 channels.
            .buffer_unordered(99999999)
            .for_each(|_| async {}),
    );

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

        let json = query_gateway(&client_web, &token).await?;
        let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone())
            .expect("Gateway response not u64");

        let max_concurrency = *&json["session_start_limit"]["max_concurrency"]
            .as_u64()
            .unwrap();

        debug!(
            "Gateway wants {} shards, {} at a time",
            total_shards, max_concurrency
        );

        server.set_concurrency(max_concurrency as usize).await;
        con.set(format!("total_shards_{}", namespace), total_shards)
            .await
            .expect("Failed to set total shards");

        if total_shards > current_shards {
            info!(
                "Gateway wants {:?} shards, but we only have {:?}",
                total_shards, current_shards
            );

            let manual_sharding: String = con
                .get("manual_sharding")
                .await?
                .unwrap_or("false".to_string());
            let manual_sharding: bool = manual_sharding.parse::<bool>()?;
            if manual_sharding {
                info!("Manual sharding enabled, doing nothing.");
                sleep(Duration::from_secs(60 * 15)).await;
                continue;
            }
            info!("Booting new shards");
            add_shards(total_shards - current_shards, max_concurrency).await;
        } else if total_shards < current_shards {
            scale_down_shards(total_shards).await;
        }
        sleep(Duration::from_secs(60 * 15)).await;
    }
}
