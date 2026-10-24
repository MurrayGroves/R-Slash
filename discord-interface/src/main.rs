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
use std::ops::{Add, Index};
use std::sync::Arc;
use std::time::SystemTime;
use std::{cmp, future};
use std::{env, fs, io::Write, thread};
use tarpc::context::Context;
use tarpc::server;
use tarpc::server::Channel;
use tarpc::tokio_serde::formats::Bincode;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, Interval, interval, sleep};

#[derive(Debug)]
struct SessionStartLimit {
    total: u64,
    remaining: u64,
    reset_after: SystemTime,
    max_concurrency: usize,
}

impl SessionStartLimit {
    fn get_and_update_remaining(&mut self) -> u64 {
        if SystemTime::now() > self.reset_after {
            self.remaining = self.total;
        }

        self.remaining
    }
}

impl Default for SessionStartLimit {
    fn default() -> Self {
        Self {
            total: Default::default(),
            remaining: Default::default(),
            reset_after: SystemTime::now(),
            max_concurrency: Default::default(),
        }
    }
}

struct ServerState {
    session_start_limit: SessionStartLimit,
    current_bucket: Option<usize>,
    starting_shards: Vec<usize>,
    last_bucket_start: Instant,
}

#[derive(Clone)]
struct Server {
    state: Arc<Mutex<ServerState>>,
}

impl DiscordInterface for Server {
    async fn request_boot(self, context: Context, shard_id: usize) -> bool {
        let mut state = self.state.lock().await;

        if state.session_start_limit.get_and_update_remaining() < 5 {
            counter!("discordinterface_rejected_boot_requests").increment(1);
            error!("Shard wanted to boot but we're out of session starts");
            return false;
        }

        if state.starting_shards.contains(&shard_id) {
            error!("Shard requested to boot which was already booting!");
            debug!(
                "Shard {} was allowed to boot because it was already booting",
                shard_id
            );
            return true;
        }

        if state.session_start_limit.max_concurrency == 0 {
            counter!("discordinterface_rejected_boot_requests").increment(1);
            error!("max_concurrency is zero!");
            return false;
        }
        let shard_bucket = shard_id / state.session_start_limit.max_concurrency;

        let allowed = if let Some(bucket) = state.current_bucket {
            debug!(
                "Current bucket is {}-{}",
                state.session_start_limit.max_concurrency * bucket,
                state.session_start_limit.max_concurrency * (bucket + 1)
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
            // Ensure we only boot a bucket every 5 seconds
            if state.last_bucket_start.elapsed().as_secs() < 5 {
                info!("Shard not allowed to boot because less than 5 secs since last bucket start");
                return false;
            }
            state.last_bucket_start = Instant::now();
            state.current_bucket = Some(shard_bucket);
            state.starting_shards.push(shard_id);
            true
        };

        let state = self.state.clone();
        if allowed {
            tokio::spawn(async move {
                sleep(Duration::from_secs(90)).await;
                let mut state = state.lock().await;
                let index = state.starting_shards.iter().position(|x| *x == shard_id);

                if let Some(index) = index {
                    warn!("Shard {} hasn't finished booting!", shard_id);
                    // state.starting_shards.remove(index);
                    // if state.starting_shards.len() == 0 {
                    //     state.current_bucket = None;
                    // }
                    counter!("discordinterface_timedout_boots").increment(1);
                } else {
                    trace!("Shard {} already finished booting, no problems", shard_id);
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

async fn scale_down_shards(new_shard_count: usize) {
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

async fn add_shards(num: usize, max_concurrency: usize) {
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
        let current_shards: usize = shards_set.status.unwrap().replicas.try_into().unwrap();

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
            let ready_shards: usize = shards_set
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
    limiter: &mut rslash_common::Limiter,
    token: &str,
) -> Result<serde_json::Value, Report> {
    loop {
        limiter.wait().await;
        let res = web_client
            .get("https://discord.com/api/v10/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await
            .context("Failed to make request to gateway")?;

        if let Err(e) = limiter.update_headers(res.headers(), res.status()).await {
            error!("Error updating limiter: {:?}", e)
        }

        let headers = format!("{:?}", res.headers());
        let text = res
            .text()
            .await
            .context("Failed to parse gateway response as text")?;

        if text == "error code: 1015\n" {
            // Hit by Cloudflare rate limit
            error!("Hit by Cloudflare rate limit!");
        } else {
            return Ok(serde_json::from_str(&text)
                .context("Failed to parse gateway response as JSON")
                .attach(text)?);
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Report> {
    env_logger::builder().init();

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
            current_bucket: None,
            starting_shards: Vec::new(),
            session_start_limit: SessionStartLimit::default(),
            last_bucket_start: Instant::now(),
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
    let mut limiter = rslash_common::Limiter::new(None, "Discord".to_string());
    loop {
        let client_k8s = kube::Client::try_default().await?;
        let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> =
            kube::Api::namespaced(client_k8s, &namespace);
        let shards_set = stateful_sets
            .get("discord-shards")
            .await
            .expect("Failed to get statefulset discord-shards");
        let current_shards = shards_set.status.unwrap().replicas as usize;

        let mut json = query_gateway(&client_web, &mut limiter, &token).await?;
        let total_shards: usize = serde_json::from_value::<usize>(json["shards"].clone())
            .expect("Gateway response not usize");

        let session_start_limit = json["session_start_limit"].take();
        let session_start_limit = SessionStartLimit {
            total: session_start_limit["total"].as_u64().unwrap(),
            remaining: session_start_limit["remaining"].as_u64().unwrap(),
            reset_after: SystemTime::now()
                + Duration::from_millis(session_start_limit["reset_after"].as_u64().unwrap()),
            max_concurrency: session_start_limit["max_concurrency"].as_u64().unwrap() as usize,
        };

        debug!(
            "Got new session start limit {:?}, reset in {:?}",
            session_start_limit,
            session_start_limit
                .reset_after
                .duration_since(SystemTime::now())
        );

        let max_concurrency = session_start_limit.max_concurrency;

        let mut state = server.state.lock().await;
        state.session_start_limit = session_start_limit;
        drop(state);

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
