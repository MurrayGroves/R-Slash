use crate::utility::Server;
use crate::{GuildListQuery, with_server};
use hmac::Hmac;
use jwt::{SignWithKey, VerifyWithKey};
use log::debug;
use serenity::all::{Guild, GuildId, GuildInfo, HttpError, User, UserId};
use serenity::http::Http;
use sha2::Sha256;
use std::convert::Infallible;
use std::sync::Arc;
use warp::hyper::StatusCode;
use warp::{Filter, Rejection, reject};

#[derive(serde::Deserialize, serde::Serialize)]
struct Claim {
    discord_id: UserId,
    /// The guilds the user is a manager of
    guilds: Vec<GuildId>,
}

#[derive(Clone)]
pub struct AuthSystem {
    key: Arc<Hmac<Sha256>>,
}

#[derive(Debug)]
pub(crate) enum AuthError {
    NoPermission(String),
    TokenValidationFailed,
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

impl AuthSystem {
    pub fn new(key: Hmac<Sha256>) -> Self {
        Self { key: Arc::new(key) }
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

    /// Create a JWT token from a Discord token
    pub async fn retrieve_discord_id(&self, discord_token: &str) -> Result<String, anyhow::Error> {
        let req = reqwest::Client::new()
            .get("https://discord.com/api/v10/users/@me")
            .bearer_auth(discord_token)
            .send()
            .await?;

        let me = req.json::<User>().await?;

        let req = reqwest::Client::new()
            .get("https://discord.com/api/v10/users/@me/guilds")
            .bearer_auth(discord_token)
            .send()
            .await?;

        let guilds = req.json::<Vec<GuildInfo>>().await?;

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

/// Reject request if the user does not have `Manage Guild` permission on all guilds
pub fn filter_manages_guilds(
    auth: AuthSystem,
) -> impl Filter<Extract = ((),), Error = Rejection> + Clone + Sized {
    with_auth(auth)
        .and(warp::query::<GuildListQuery>())
        .and(warp::header::<String>("Authorization"))
        .and_then(
            |auth: AuthSystem, guild_list_query: GuildListQuery, token: String| async move {
                let token = token.trim_start_matches("Bearer ");
                let guilds: Vec<GuildId> = guild_list_query.into();

                let verifications = guilds
                    .iter()
                    .map(|guild| auth.verify_guild(&token, guild))
                    .collect::<Result<Vec<_>, _>>();

                if let Err(err) = verifications {
                    debug!("Error verifying guilds: {:?}", err);
                    return Err(reject::custom(AuthError::TokenValidationFailed));
                }

                if verifications.unwrap().iter().all(|x| *x) {
                    debug!("User has permission to manage all guilds");
                    return Ok(());
                } else {
                    Err(reject::custom(crate::auth::AuthError::NoPermission(
                        format!(
                            "User does not have permission to manage some of guilds {:?}",
                            guilds
                        ),
                    )))
                }
            },
        )
}
