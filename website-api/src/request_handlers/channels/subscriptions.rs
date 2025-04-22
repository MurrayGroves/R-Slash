use crate::auth::filter_channel_belongs_to_managed_guild;
use crate::utility::{GenericError, Server};
use crate::{auth, utility, with_server};
use anyhow::anyhow;
use log::error;
use rslash_common::Bot;
use tarpc::context::Context;
use warp::{Filter, reject};

/// GET /api/{bot}/user/channel/{channel_id}/subscriptions
pub async fn get_subscriptions_for_channel(
    bot: Bot,
    channel_id: u64,
    server: Server,
) -> Result<impl warp::Reply, warp::Rejection> {
    let subscriptions = server
        .post_subscriber
        .list_subscriptions(Context::current(), channel_id, bot)
        .await
        .map_err(|err| {
            error!("Error fetching subscriptions: {}", err);
            reject::custom(GenericError::new(anyhow!(err)))
        })?.map_err(|err| {
        error!("Error fetching subscriptions: {}", err);
        reject::custom(GenericError::new(anyhow!(err)))
    })?;

    Ok(warp::reply::json(&subscriptions))
}

pub fn routes(
    server: Server,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let get_subscriptions = warp::path!("api" / Bot / "channel" / u64 / "subscriptions")
        .and(warp::get())
        .and(warp::path::end())
        .and(with_server(server))
        .and(warp::cookie("token"))
        .and_then(|bot: Bot, channel_id: u64, server: Server, token: String| async move {
            if let Err(err) = filter_channel_belongs_to_managed_guild(&server, channel_id, bot, token).await {
                return Err(err);
            }
            get_subscriptions_for_channel(bot, channel_id, server).await
        });

    get_subscriptions
}
