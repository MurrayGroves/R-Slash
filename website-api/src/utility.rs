use crate::auth::AuthSystem;
use serenity::all::HttpError::UnsuccessfulRequest;
use serenity::all::{ErrorResponse, GuildChannel, GuildId, StatusCode};
use serenity::http::Http;
use std::sync::Arc;

#[derive(Clone)]
pub struct Server {
    pub http: Arc<Http>,
    pub auth: AuthSystem,
}

pub async fn get_channels_for_guild(
    guild_id: &GuildId,
    server: &Server,
) -> Result<Option<Vec<GuildChannel>>, anyhow::Error> {
    let channels = match server.http.get_channels(*guild_id).await {
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
