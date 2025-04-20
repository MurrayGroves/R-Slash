use crate::auth::{Claim, filter_channel_belongs_to_managed_guild, get_user_claim};
use crate::request_handlers::channels::subscriptions::get_subscriptions_for_channel;
use crate::utility::{GenericError, Server};
use crate::with_server;
use anyhow::anyhow;
use log::error;
use redis::AsyncTypedCommands;
use rslash_common::Bot;
use serenity::all::{GuildInfo, UserId};
use warp::{Filter, reject};

/// Fetches the user for a given user from Redis.
async fn get_guilds(user_id: UserId, server: &Server) -> Result<Vec<GuildInfo>, anyhow::Error> {
    let json_vec = server
        .auth
        .redis
        .clone()
        .lrange(format!("website:users:{}:guilds", user_id.get()), 0, -1)
        .await?;

    let guilds: Vec<GuildInfo> = json_vec
        .iter()
        .map(|json| {
            serde_json::from_str::<GuildInfo>(&json)
                .map_err(|e| anyhow::anyhow!("Failed to deserialize guild: {}", e))
        })
        .collect::<Result<Vec<GuildInfo>, anyhow::Error>>()?;
    Ok(guilds)
}

pub fn routes(
    server: Server,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let get_guilds = warp::path!("api" / "guilds")
        .and(warp::get())
        .and(warp::path::end())
        .and(get_user_claim(server.clone()))
        .and(with_server(server))
        .and_then(|claim: Claim, server: Server| async move {
            match get_guilds(claim.discord_id, &server).await {
                Ok(guilds) => Ok(warp::reply::json(&guilds)),
                Err(err) => {
                    error!("Error fetching user: {}", err);
                    Err(reject::custom(GenericError::new(err)))
                }
            }
        });

    get_guilds
}
