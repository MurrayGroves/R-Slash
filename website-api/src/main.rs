mod utility;

use crate::utility::{Server, get_channels_for_guild};
use log::error;
use serenity::all::{Channel, GuildChannel, GuildId};
use serenity::futures;
use serenity::http::ErrorResponse;
use serenity::http::HttpError::UnsuccessfulRequest;
use std::collections::HashMap;
use std::sync::Arc;
use warp::Filter;
use warp::http::StatusCode;

async fn get_channels_for_guilds(
    guilds: String,
    server: Server,
) -> Result<impl warp::Reply, warp::Rejection> {
    let guilds = guilds
        .split(',')
        .map(|s| s.parse::<u64>().unwrap())
        .collect::<Vec<u64>>();
    let futures = guilds.iter().map(|id| get_channels_for_guild(*id, &server));

    let results: Vec<Result<Option<Vec<GuildChannel>>, anyhow::Error>> =
        futures::future::join_all(futures).await;

    let mut channels: HashMap<GuildId, Vec<GuildChannel>> = HashMap::new();
    for result in results {
        match result {
            Ok(Some(guild_channels)) => {
                if guild_channels.len() != 0 {
                    channels.insert(guild_channels[0].guild_id, guild_channels);
                }
            }
            Ok(None) => {}
            Err(err) => {
                error!("Error fetching channels: {}", err);
            }
        }
    }

    Ok(warp::reply::json(&channels))
}

fn with_server(
    server: Server,
) -> impl Filter<Extract = (Server,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || server.clone())
}

#[derive(serde::Deserialize)]
struct GuildListQuery {
    guilds: String,
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let server = utility::Server {
        http: Arc::new(serenity::http::Http::new(
            &*std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN must be set"),
        )),
    };

    let get_channels = warp::path!("api" / "guilds" / "batch" / "channels")
        .and(warp::get())
        .and(with_server(server))
        .and(warp::query::<GuildListQuery>())
        .and_then(|server, body: GuildListQuery| get_channels_for_guilds(body.guilds, server));

    let handler = get_channels.with(warp::log("website_api")).with(
        warp::cors()
            .allow_any_origin()
            .allow_origin("https://rsla.sh")
            .allow_origin("http://localhost:5173")
            .allow_origin("http://localhost:8080")
            .allow_methods(vec!["GET", "POST", "OPTIONS"])
            .allow_headers(vec![
                "User-Agent",
                "Sec-Fetch-Mode",
                "Referer",
                "Origin",
                "Access-Control-Request-Method",
                "Access-Control-Request-Headers",
                "Access-Control-Allow-Origin",
                "content-type",
            ]),
    );

    warp::serve(handler).run(([0, 0, 0, 0], 8080)).await;
}
