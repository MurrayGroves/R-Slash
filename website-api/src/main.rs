mod auth;
mod utility;

use crate::auth::{AuthSystem, filter_manages_guilds};
use crate::utility::{Server, get_channels_for_guild};
use hmac::Mac;
use log::{debug, error};
use serenity::all::{Channel, GuildChannel, GuildId};
use serenity::futures;
use serenity::http::ErrorResponse;
use serenity::http::HttpError::UnsuccessfulRequest;
use std::collections::HashMap;
use std::sync::Arc;
use warp::http::StatusCode;
use warp::{Filter, Rejection, reject};

async fn get_channels_for_guilds(
    guilds: Vec<GuildId>,
    server: Server,
) -> Result<impl warp::Reply, warp::Rejection> {
    let futures = guilds.iter().map(|id| get_channels_for_guild(id, &server));

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

#[derive(Debug)]
struct GenericError {
    error: anyhow::Error,
}

impl GenericError {
    fn new(error: anyhow::Error) -> Self {
        Self { error }
    }
}

impl warp::reject::Reject for GenericError {}

async fn handle_generic_error(err: Rejection) -> Result<impl warp::Reply, warp::Rejection> {
    if let Some(generic_error) = err.find::<GenericError>() {
        error!("Generic error: {:?}", generic_error.error);
        Ok(warp::reply::with_status(
            "Internal server error",
            StatusCode::INTERNAL_SERVER_ERROR,
        ))
    } else {
        error!("Unhandled rejection: {:?}", err);
        Ok(warp::reply::with_status(
            "Internal server error",
            StatusCode::INTERNAL_SERVER_ERROR,
        ))
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let server = Server {
        http: Arc::new(serenity::http::Http::new(
            &*std::env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN must be set"),
        )),
        auth: AuthSystem::new(
            hmac::Hmac::<sha2::Sha256>::new_from_slice(
                (&*std::env::var("JWT_SECRET").expect("JWT_SECRET must be set")).as_bytes(),
            )
            .expect("Invalid JWT secret"),
        ),
    };

    let get_channels = warp::path!("api" / "guilds" / "batch" / "channels")
        .and(warp::get())
        .and(filter_manages_guilds(server.auth.clone()))
        .and(with_server(server.clone()))
        .and(warp::query::<GuildListQuery>())
        .and_then(|_, server, body: GuildListQuery| get_channels_for_guilds(body.into(), server))
        .recover(auth::rejection_handler);

    let login = warp::path!("api" / "login")
        .and(warp::post())
        .and(with_server(server.clone()))
        .and(warp::body::json())
        .and_then(|server: Server, body: String| async move {
            let token = match server.auth.retrieve_discord_id(&body).await {
                Ok(token) => token,
                Err(err) => {
                    error!("Error retrieving Discord ID: {}", err);
                    return Err(reject::custom(GenericError::new(err)));
                }
            };
            Ok(warp::reply::json(&token))
        })
        .recover(auth::rejection_handler)
        .recover(handle_generic_error);

    let handler = get_channels.or(login).with(warp::log("website_api")).with(
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
                "authorization",
            ]),
    );

    warp::serve(handler).run(([0, 0, 0, 0], 8080)).await;
}
