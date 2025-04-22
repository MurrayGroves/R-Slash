use crate::utility::{GenericError, Server};
use crate::{GuildListQuery, with_server};
use anyhow::anyhow;
use hmac::Hmac;
use jwt::{SignWithKey, VerifyWithKey};
use log::{debug, error};
use redis::AsyncTypedCommands;
use redis::aio::MultiplexedConnection;
use rslash_common::Bot;
use serde_json::json;
use serenity::all::{
    Channel, ChannelId, Guild, GuildId, GuildInfo, HttpError, PartialGuild, User, UserId,
};
use serenity::futures;
use serenity::http::Http;
use sha2::Sha256;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::join;
use warp::hyper::StatusCode;
use warp::{Filter, Rejection, reject};

#[derive(serde::Deserialize, serde::Serialize)]
pub struct Claim {
    pub discord_id: UserId,
    /// The user the user is a manager of
    pub guilds: Vec<GuildId>,
}

#[derive(Clone)]
pub struct AuthSystem {
    key: Arc<Hmac<Sha256>>,
    pub redis: MultiplexedConnection,
}

#[derive(Debug)]
pub(crate) enum AuthError {
    NoPermission(String),
    TokenValidationFailed,
    /// Failed to fetch the resource that authorisation was being checked against
    FailedToFetchResource,
    /// Some logic failure in the auth system
    InternalError,
}

pub(crate) async fn rejection_handler(err: Rejection) -> Result<impl warp::Reply, Rejection> {
    if let Some(AuthError::NoPermission(msg)) = err.find() {
        return Ok(warp::reply::with_status(msg.clone(), StatusCode::FORBIDDEN));
    }

    if let Some(AuthError::TokenValidationFailed) = err.find() {
        debug!("Token validation failed");
        return Ok(warp::reply::with_status(
            "Token validation failed".to_string(),
            StatusCode::UNAUTHORIZED,
        ));
    }
    Err(err)
}

impl reject::Reject for AuthError {}

#[derive(serde::Deserialize, serde::Serialize)]
struct DiscordToken {
    access_token: String,
    expires_at: u64,
    refresh_token: String,
}

impl AuthSystem {
    pub async fn new(redis: MultiplexedConnection, key: Hmac<Sha256>) -> Self {
        Self {
            key: Arc::new(key),
            redis,
        }
    }

    /// Verify a JWT token
    fn verify_claim(&self, token: &str) -> Result<Claim, anyhow::Error> {
        let claim = token.verify_with_key(&*self.key)?;
        Ok(claim)
    }

    /// Verify user is a manager of the guild
    pub(crate) fn verify_guild(
        &self,
        token: &str,
        guild_id: &GuildId,
    ) -> Result<bool, anyhow::Error> {
        let claim = self.verify_claim(token)?;
        if claim.guilds.contains(guild_id) {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Login a user by their Discord OAuth code. Returns a JWT token for our backend.
    pub async fn login_to_discord(
        &self,
        bot: Bot,
        server: &Server,
        client_id: &str,
        client_secret: &str,
        discord_code: &str,
        redirect_uri: &str,
    ) -> Result<String, anyhow::Error> {
        debug!("Client sec {}", client_secret);
        let client = reqwest::Client::new();
        let req = client
            .post("https://discord.com/api/oauth2/token")
            .form(&json!({
                "grant_type": "authorization_code",
                "code": discord_code,
                "redirect_uri": redirect_uri,
            }))
            .basic_auth(client_id, Some(client_secret))
            .send()
            .await?;

        let json: serde_json::Value = req.json().await?;
        debug!("Discord token response: {:?}", json);
        let discord_token = DiscordToken {
            access_token: json
                .get("access_token")
                .ok_or(anyhow!("Missing access_token"))?
                .as_str()
                .ok_or(anyhow!("Access token not string"))?
                .to_string(),
            expires_at: json
                .get("expires_in")
                .ok_or(anyhow!("Missing expires_in"))?
                .as_u64()
                .ok_or(anyhow!("expires_in not u64"))?
                + chrono::Utc::now().timestamp() as u64,
            refresh_token: json
                .get("refresh_token")
                .ok_or(anyhow!("Missing refresh_token"))?
                .as_str()
                .ok_or(anyhow!("Refresh token not string"))?
                .to_string(),
        };

        // Send requests concurrently
        let (me_req, guilds_req) = join!(
            reqwest::Client::new()
                .get("https://discord.com/api/v10/users/@me")
                .bearer_auth(&discord_token.access_token)
                .send(),
            reqwest::Client::new()
                .get("https://discord.com/api/v10/users/@me/guilds")
                .bearer_auth(&discord_token.access_token)
                .send(),
        );

        let me = me_req?.text().await?;
        let me: User = match serde_json::from_str(&me) {
            Ok(user) => user,
            Err(_) => {
                return Err(anyhow::anyhow!("Failed to parse user response, {}", me));
            }
        };

        let guilds = guilds_req?.text().await?;
        let guilds: Vec<GuildInfo> = match serde_json::from_str(&guilds) {
            Ok(guilds) => guilds,
            Err(_) => {
                return Err(anyhow::anyhow!("Failed to parse user response, {}", guilds));
            }
        };

        // Filter guilds to only those that the user has `Manage Guild` or `Administrator` permission
        let guilds = guilds
            .into_iter()
            .filter(|guild| guild.permissions.manage_guild() || guild.permissions.administrator())
            .collect::<Vec<_>>();

        // Filter guilds to only those that the bot is in
        let http = &server.http[&bot];
        let futures = guilds.iter().map(|guild| http.get_guild(guild.id));
        let bot_guilds: Vec<Result<PartialGuild, _>> = futures::future::join_all(futures).await;
        let bot_guilds: Vec<PartialGuild> = bot_guilds
            .into_iter()
            .filter_map(|guild| match guild {
                Ok(guild) => Some(guild),
                Err(err) => {
                    debug!("Error fetching guild: {:?}", err);
                    None
                }
            })
            .collect();

        // Store Guilds in Redis for faster lookup later
        let guilds_json = guilds
            .iter()
            .filter(|guild| bot_guilds.iter().any(|bot_guild| bot_guild.id == guild.id))
            .filter_map(|guild| serde_json::to_string(guild).ok())
            .collect::<Vec<_>>();

        let key = format!("website:users:{}:guilds", me.id);
        let mut redis = self.redis.clone();
        redis.del(&key).await?;
        redis.lpush(key, guilds_json).await?;

        let claim = Claim {
            discord_id: me.id,
            guilds: guilds.into_iter().map(|guild| guild.id).collect(),
        };

        Ok(claim.sign_with_key(&*self.key)?)
    }
}

fn with_auth(
    auth: AuthSystem,
) -> impl Filter<Extract = (AuthSystem,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || auth.clone())
}

/// Reject request if the user does not have `Manage Guild` permission on all user
pub fn filter_manages_guilds(
    auth: AuthSystem,
) -> impl Filter<Extract = ((),), Error = Rejection> + Clone + Sized {
    with_auth(auth)
        .and(warp::query::<GuildListQuery>())
        .and(warp::cookie("token"))
        .and_then(
            |auth: AuthSystem, guild_list_query: GuildListQuery, token: String| async move {
                let guilds: Vec<GuildId> = guild_list_query.into();

                let verifications = guilds
                    .iter()
                    .map(|guild| auth.verify_guild(&token, guild))
                    .collect::<Result<Vec<_>, _>>();

                if let Err(err) = verifications {
                    debug!("Error verifying user: {:?}", err);
                    return Err(reject::custom(AuthError::TokenValidationFailed));
                }

                if verifications.unwrap().iter().all(|x| *x) {
                    debug!("User has permission to manage all guilds");
                    return Ok(());
                } else {
                    Err(reject::custom(crate::auth::AuthError::NoPermission(
                        format!(
                            "User does not have permission to manage some guilds {:?}",
                            guilds
                        ),
                    )))
                }
            },
        )
}

pub async fn filter_channel_belongs_to_managed_guild(
    server: &Server,
    channel_id: u64,
    bot: Bot,
    token: String,
) -> Result<(), Rejection> {
    debug!(
        "Checking if user has permission to access channel {}",
        channel_id
    );
    let channel = server.http[&bot]
        .get_channel(channel_id.into())
        .await
        .map_err(|err| {
            debug!("Error fetching channel: {:?}", err);
            reject::custom(AuthError::FailedToFetchResource)
        })?;

    let verification = match channel {
        Channel::Guild(guild_channel) => {
            let guild_id = guild_channel.guild_id;
            match server.auth.verify_guild(&token, &guild_id) {
                Ok(true) => Ok(true),
                Ok(false) => Err(reject::custom(AuthError::NoPermission(
                    "User does not have permission to access this channel".to_string(),
                ))),
                Err(_) => Err(reject::custom(AuthError::TokenValidationFailed)),
            }
        }
        Channel::Private(private_channel) => {
            let authed_user = match server.auth.verify_claim(&token) {
                Ok(claim) => claim.discord_id,
                Err(_) => {
                    return Err(reject::custom(AuthError::TokenValidationFailed));
                }
            };
            if private_channel.recipient.id == authed_user {
                Ok(true)
            } else {
                Err(reject::custom(AuthError::NoPermission(
                    "User does not have permission to access this channel".to_string(),
                )))
            }
        }
        _ => {
            error!("Encountered unknown channel type");
            Err(reject::custom(AuthError::InternalError))
        }
    };

    match verification {
        Ok(true) => Ok(()),
        Ok(false) => Err(reject::custom(AuthError::NoPermission(
            "User does not have permission to access this channel".to_string(),
        ))),
        Err(err) => {
            debug!("Error verifying channel: {:?}", err);
            Err(err)
        }
    }
}

/// Retrieve logged-in user's claim
pub fn get_user_claim(
    server: Server,
) -> impl Filter<Extract = (Claim,), Error = Rejection> + Clone + Sized {
    with_server(server)
        .and(warp::cookie::optional("token"))
        .and_then(|server: Server, token: Option<String>| async move {
            if token.is_none() {
                return Err(reject::custom(AuthError::NoPermission(
                    "Token not provided".to_string(),
                )));
            }

            let claim = server.auth.verify_claim(&token.unwrap());
            claim.map_err(|err| {
                error!("Token validation failed due to error: {:?}", err);
                reject::custom(AuthError::TokenValidationFailed)
            })
        })
}

#[derive(serde::Deserialize)]
struct LoginBody {
    code: String,
    redirect_uri: String,
}

pub fn routes(
    server: Server,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let login = warp::path!("api" / Bot / "login")
        .and(warp::post())
        .and(with_server(server.clone()))
        .and(warp::body::json())
        .and_then(|bot: Bot, mut server: Server, body: LoginBody| async move {
            let token = match server
                .auth
                .login_to_discord(
                    bot,
                    &server,
                    &server.bot_credentials[&bot].id,
                    &server.bot_credentials[&bot].secret,
                    &body.code,
                    &body.redirect_uri,
                )
                .await
            {
                Ok(token) => token,
                Err(err) => {
                    error!("Error retrieving Discord ID: {:?}", err);
                    return Err(reject::custom(GenericError::new(err)));
                }
            };
            Ok(warp::reply::with_header(
                warp::reply::reply(),
                "Set-Cookie",
                format!(
                    "token={}; Path=/; HttpOnly; SameSite=Strict; Secure; Max-Age=2592000;",
                    token
                ),
            ))
        });

    let check = warp::path!("api" / "login" / "check")
        .and(warp::get())
        .and(get_user_claim(server.clone()))
        .map(|_| warp::reply::with_status("Authenticated", StatusCode::OK));

    login.or(check)
}
