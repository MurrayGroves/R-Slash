use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use auto_poster::AutoPosterClient;
use log::{debug, warn};
use memberships::get_user_tiers_from_ctx;
use rslash_types::{InteractionResponse, InteractionResponseMessage};
use serenity::all::{ActionRowComponent, Context, CreateEmbed, ModalInteraction};
use tracing::instrument;

use crate::{capture_event, discord::ResponseTracker};

#[instrument(skip(ctx, modal, custom_id, tracker))]
pub async fn autopost_create<'a>(
    ctx: &Context,
    modal: &ModalInteraction,
    custom_id: HashMap<String, serde_json::Value>,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    capture_event(
        ctx.data.clone(),
        "autopost_start",
        None,
        modal.guild_id,
        Some(modal.channel_id),
        &format!("user_{}", modal.user.id.get()),
    )
    .await;

    let search = match &custom_id["search"] {
        serde_json::value::Value::Null => None,
        serde_json::value::Value::String(x) => Some(x.clone()),
        _ => {
            warn!("Invalid search: {:?}", custom_id["search"]);
            None
        }
    };

    let mut interval = String::new();
    let mut limit = None;

    for row in &modal.data.components {
        for comp in &row.components {
            match comp {
                ActionRowComponent::InputText(input) => {
                    if input.custom_id == "delay" {
                        interval = match input.value.clone() {
                            Some(x) => x,
                            _ => "5s".to_string(),
                        };
                    } else if input.custom_id == "limit" {
                        limit = match input.value.clone() {
                            Some(x) => Some(x),
                            _ => Some("10".to_string()),
                        };
                    }
                }
                _ => {}
            }
        }
    }

    let is_premium = get_user_tiers_from_ctx(ctx, modal.user.id.get())
        .await
        .bronze
        .active;

    let limit = match limit {
        Some(x) => match x.parse::<u32>() {
            Ok(x) => Some(x),
            Err(_) => {
                if x == "infinite" && is_premium {
                    None
                } else {
                    debug!("Invalid limit: {:?}", x);
                    let error_message = if x == "infinite" {
                        "You must be a premium user to set the limit to infinite.\n[Buy Premium](https://ko-fi.com/rslash)"
                    } else {
                        "Invalid limit, must be a number."
                    };

                    return tracker
                        .send_response(InteractionResponse::Message(InteractionResponseMessage {
                            embed: Some(
                                CreateEmbed::new()
                                    .title("Invalid Limit")
                                    .description(error_message)
                                    .color(0xff0000),
                            ),
                            ephemeral: true,
                            ..Default::default()
                        }))
                        .await;
                }
            }
        },
        None => None,
    };

    macro_rules! invalid_interval_resp {
        ($interval:expr) =>
        { return tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
            embed: Some(
                CreateEmbed::new()
                    .title("Invalid Interval")
                    .description(
                        format!("Invalid Interval: {}, it must be a number followed by either 's', 'm', 'h', or 'd' - indicating seconds, minutes, hours, or days.", $interval),
                    )
                    .color(0xff0000),
            ),
            ephemeral: true,
            ..Default::default()
        })).await;}
    }

    let multiplier = if interval.ends_with("s") {
        1
    } else if interval.ends_with("m") {
        60
    } else if interval.ends_with("h") {
        3600
    } else if interval.ends_with("d") {
        86400
    } else {
        invalid_interval_resp!(interval);
    };

    let interval_parsed = if interval.ends_with("s") {
        interval.replace("s", "").parse::<u64>()
    } else if interval.ends_with("m") {
        interval.replace("m", "").parse::<u64>()
    } else if interval.ends_with("h") {
        interval.replace("h", "").parse::<u64>()
    } else if interval.ends_with("d") {
        interval.replace("d", "").parse::<u64>()
    } else {
        invalid_interval_resp!(interval);
    };

    if interval_parsed == Ok(0) {
        invalid_interval_resp!(interval);
    }

    let interval = match interval_parsed {
        Ok(x) => tokio::time::Duration::from_secs(x * (multiplier as u64)),
        Err(_) => {
            invalid_interval_resp!(interval);
        }
    };

    let lock = ctx.data.read().await;

    let autoposter = lock
        .get::<AutoPosterClient>()
        .ok_or(anyhow!("Autoposter client not found"))?;

    match autoposter
        .register_autopost(
            tarpc::context::current(),
            custom_id["subreddit"].to_string().replace('"', ""),
            modal.channel_id.get(),
            interval,
            limit,
            search,
            ctx.http.application_id().expect("app ID missing").get(),
        )
        .await?
    {
        Ok(_) => {}
        Err(e) => {
            bail!(e);
        }
    };

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            embed: Some(
                CreateEmbed::new()
                    .title("Autopost Loop Started")
                    .description("Autopost loop started!")
                    .color(0x00ff00),
            ),
            ephemeral: true,
            ..Default::default()
        }))
        .await
}
