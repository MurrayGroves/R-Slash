use crate::auth::AuthSystem;
use hmac::Mac;
use log::error;
use post_subscriber::SubscriberClient;
use rslash_common::Bot;
use serenity::all::HttpError::UnsuccessfulRequest;
use serenity::all::{ErrorResponse, GuildChannel, GuildId, StatusCode};
use serenity::http::Http;
use std::collections::HashMap;
use std::iter;
use std::sync::Arc;
use std::time::Duration;
use strum::IntoEnumIterator;
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::client;
use tarpc::serde_transport::Transport;
use tarpc::tokio_serde::formats::Bincode;
use warp::Rejection;

#[derive(Clone)]
pub struct BotCredentials {
    pub id: String,
    pub secret: String,
}

#[derive(Clone)]
pub struct Server {
    pub http: Arc<HashMap<Bot, Http>>,
    pub auth: AuthSystem,
    pub post_subscriber: SubscriberClient,
    pub bot_credentials: Arc<HashMap<Bot, BotCredentials>>,
}

impl Server {
    pub async fn new() -> Result<Server, anyhow::Error> {
        let reconnect_opts = ReconnectOptions::new()
            .with_exit_if_first_connect_fails(false)
            .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
        let tcp_stream =
            StubbornTcpStream::connect_with_options("post-subscriber:50051", reconnect_opts)
                .await?;
        let transport = Transport::from((tcp_stream, Bincode::default()));

        let mut bot_credentials = HashMap::new();
        let mut bot_http = HashMap::new();
        for bot in Bot::iter() {
            let id_name = format!("{}_ID", bot.to_string().to_uppercase());
            let token_name = format!("{}_TOKEN", bot.to_string().to_uppercase());
            let secret_name = format!("{}_SECRET", bot.to_string().to_uppercase());

            let id = std::env::var(&id_name).expect(&format!("{} must be set", id_name));
            let token = std::env::var(&token_name).expect(&format!("{} must be set", token_name));
            let secret =
                std::env::var(&secret_name).expect(&format!("{} must be set", secret_name));

            bot_credentials.insert(bot, BotCredentials { id, secret });
            let http = Http::new(&token);
            bot_http.insert(bot, http);
        }

        let subscriber = SubscriberClient::new(client::Config::default(), transport).spawn();

        let redis_client = redis::Client::open("redis://discord-bot-shared-redis/")?;
        let redis = redis_client
            .get_multiplexed_async_connection()
            .await
            .expect("Can't connect to redis");

        Ok(Server {
            http: Arc::new(bot_http),
            auth: AuthSystem::new(
                redis,
                hmac::Hmac::<sha2::Sha256>::new_from_slice(
                    (&*std::env::var("JWT_SECRET").expect("JWT_SECRET must be set")).as_bytes(),
                )
                .expect("Invalid JWT secret"),
            )
            .await,
            post_subscriber: subscriber,
            bot_credentials: Arc::new(bot_credentials),
        })
    }
}

pub async fn get_channels_for_guild(
    bot: Bot,
    guild_id: &GuildId,
    server: &Server,
) -> Result<Option<Vec<GuildChannel>>, anyhow::Error> {
    let channels = match server.http[&bot].get_channels(*guild_id).await {
        Ok(channels) => channels,
        Err(err) => {
            let err_string = format!("{:?}", err);
            if let serenity::Error::Http(UnsuccessfulRequest(ErrorResponse { error, .. })) = err {
                if error.code == 50001 {
                    return Ok(None);
                }
            };
            anyhow::bail!(
                "Failed to get channels for guild {}: {}",
                guild_id,
                err_string
            );
        }
    };
    Ok(Some(channels))
}

#[derive(Debug)]
pub struct GenericError {
    error: anyhow::Error,
}

impl GenericError {
    pub(crate) fn new(error: anyhow::Error) -> Self {
        Self { error }
    }
}

impl warp::reject::Reject for GenericError {}

pub async fn handle_generic_error(err: Rejection) -> Result<impl warp::Reply, warp::Rejection> {
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
