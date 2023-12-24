// HTTP
use rouille::Request;
use rouille::Response;

// Logging
use log::*;
use std::io::Write;

// Redis
use redis::{Commands, from_redis_value, Value};

// Misc
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};


fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    info!("Starting badge server");

    let con = Arc::new(Mutex::new(redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap()));

    rouille::start_server("0.0.0.0:80", move |_request| {
        info!("Request received");
        let mut con = match con.lock() {
            Ok(val) => val,
            Err(e) => {
                error!("Error locking redis connection: {}", e);
                return Response::text("Error locking redis connection");
            }
        };
        let guild_counts_rslash: HashMap<String, redis::Value> = match con.hgetall("shard_guild_counts_r-slash") {
            Ok(val) => val,
            Err(e) => {
                error!("Error getting shard guild counts: {}", e);
                return Response::text("Error getting shard guild counts");
            }
        };

        let mut guild_count = 0;
        for (_, count) in guild_counts_rslash {
            guild_count += match from_redis_value::<u64>(&count) {
                Ok(val) => val,
                Err(e) => {
                    error!("Error parsing shard guild count: {}", e);
                    return Response::text("Error parsing shard guild count");
                }
            };
        }

        let guild_counts_bootybot: HashMap<String, redis::Value> = match con.hgetall("shard_guild_counts_booty-bot") {
            Ok(val) => val,
            Err(e) => {
                error!("Error getting shard guild counts: {}", e);
                return Response::text("Error getting shard guild counts");
            }
        };

        for (_, count) in guild_counts_bootybot {
            guild_count += match from_redis_value::<u64>(&count) {
                Ok(val) => val,
                Err(e) => {
                    error!("Error parsing shard guild count: {}", e);
                    return Response::text("Error parsing shard guild count");
                }
            };
        }

        info!("Guild count: {}", guild_count);
        Response::text(format!("{{\"guild_count\": {}}}", guild_count))
    });
}
