mod auth;
mod request_handlers;
mod utility;

use crate::auth::{AuthSystem, filter_manages_guilds};
#[macro_use]
use crate::utility::{Server, get_channels_for_guild};
use crate::utility::GenericError;
use anyhow::anyhow;
use hmac::Mac;
use log::{debug, error};
use rslash_common::Bot;
use serenity::all::{Channel, GuildChannel, GuildId};
use serenity::futures;
use serenity::http::ErrorResponse;
use serenity::http::HttpError::UnsuccessfulRequest;
use std::collections::HashMap;
use std::sync::Arc;
use tarpc::context::Context;
use warp::http::{HeaderValue, StatusCode};
use warp::hyper::HeaderMap;
use warp::{Filter, Rejection, reject};

async fn get_channels_for_guilds(
    bot: Bot,
    guilds: Vec<GuildId>,
    server: Server,
) -> Result<impl warp::Reply, warp::Rejection> {
    let futures = guilds
        .iter()
        .map(|id| get_channels_for_guild(bot, id, &server));

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

/// Deserializer for the guild list query parameter
#[derive(serde::Deserialize)]
pub struct GuildListQuery {
    guilds: String,
}

impl Into<Vec<GuildId>> for GuildListQuery {
    fn into(self) -> Vec<GuildId> {
        self.guilds
            .split(',')
            .map(|s| s.parse::<u64>().unwrap())
            .map(GuildId::new)
            .collect()
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let server = Server::new().await.expect("Failed to create server");

    let get_channels = warp::path!("api" / Bot / "guild" / "batch" / "channels")
        .and(warp::get())
        .and(filter_manages_guilds(server.auth.clone()))
        .and(with_server(server.clone()))
        .and(warp::query::<GuildListQuery>())
        .and_then(|bot: Bot, _, server, body: GuildListQuery| {
            get_channels_for_guilds(bot, body.into(), server)
        });

    let handler = get_channels
        .or(request_handlers::channels::subscriptions::routes(
            server.clone(),
        ))
        .or(auth::routes(server.clone()))
        .or(request_handlers::user::guilds::routes(server.clone()))
        .recover(auth::rejection_handler)
        .recover(utility::handle_generic_error)
        .with(warp::log("website_api"))
        .with(
            warp::cors()
                .allow_any_origin()
                .allow_origin("https://rsla.sh")
                .allow_origin("http://localhost:5173")
                .allow_origin("http://localhost:8080")
                .allow_methods(vec!["GET", "POST", "OPTIONS"])
                .allow_credentials(true)
                .allow_headers(vec![
                    "User-Agent",
                    "Sec-Fetch-Mode",
                    "Referer",
                    "Origin",
                    "Access-Control-Request-Method",
                    "Access-Control-Request-Headers",
                    "Access-Control-Allow-Origin",
                    "Access-Control-Allow-Credentials",
                    "content-type",
                    "Cookie",
                    "Set-Cookie",
                ]),
        );

    warp::serve(handler).run(([0, 0, 0, 0], 8080)).await;
}
