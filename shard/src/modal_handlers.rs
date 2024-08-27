use std::collections::HashMap;

use anyhow::anyhow;
use auto_poster::AutoPosterClient;
use connection_pooler::ResourceManager;
use log::{debug, error, warn};
use memberships::get_user_tiers;
use serenity::all::{
    ActionRowComponent, Context, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseMessage, ModalInteraction,
};

use crate::capture_event;

pub async fn autopost_create(
    ctx: &Context,
    modal: ModalInteraction,
    custom_id: HashMap<String, serde_json::Value>,
    modal_tx: &sentry::TransactionOrSpan,
) -> Result<(), anyhow::Error> {
    capture_event(
        ctx.data.clone(),
        "autopost_start",
        Some(&modal_tx),
        None,
        &format!(
            "channel_{}",
            modal.channel.clone().unwrap().id.get().to_string()
        ),
    )
    .await;

    let lock = ctx.data.read().await;

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

    let is_premium = get_user_tiers(
        modal.user.id.get().to_string(),
        &mut *lock
            .get::<ResourceManager<mongodb::Client>>()
            .unwrap()
            .get_available_resource()
            .await
            .lock()
            .await,
        None,
    )
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

                    let error_response = CreateInteractionResponseMessage::new()
                        .embed(
                            CreateEmbed::new()
                                .title("Invalid Limit")
                                .description(error_message)
                                .color(0xff0000),
                        )
                        .ephemeral(true);

                    match modal
                        .create_response(
                            &ctx.http,
                            CreateInteractionResponse::Message(error_response),
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            error!("Failed to send error response: {:?}", e);
                        }
                    };
                    return Ok(());
                }
            }
        },
        None => None,
    };

    let invalid_interval = || async {
        debug!("Invalid interval: {:?}", interval);

        let error_response = CreateInteractionResponseMessage::new()
            .embed(CreateEmbed::new()
                .title("Invalid Interval")
                .description(format!("Invalid Interval: {}, it must be a number followed by either 's', 'm', 'h', or 'd' - indicating seconds, minutes, hours, or days.", interval))
                .color(0xff0000)
            )
            .ephemeral(true);

        match modal
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Message(error_response),
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                error!("Failed to send error response: {:?}", e);
            }
        };
    };

    let multiplier = if interval.ends_with("s") {
        1
    } else if interval.ends_with("m") {
        60
    } else if interval.ends_with("h") {
        3600
    } else if interval.ends_with("d") {
        86400
    } else {
        invalid_interval().await;
        return Ok(());
    };

    let interval = if interval.ends_with("s") {
        interval.replace("s", "").parse::<u64>()
    } else if interval.ends_with("m") {
        interval.replace("m", "").parse::<u64>()
    } else if interval.ends_with("h") {
        interval.replace("h", "").parse::<u64>()
    } else if interval.ends_with("d") {
        interval.replace("d", "").parse::<u64>()
    } else {
        invalid_interval().await;
        return Ok(());
    };

    let interval = match interval {
        Ok(x) => tokio::time::Duration::from_secs(x * (multiplier as u64)),
        Err(_) => {
            invalid_interval().await;
            return Ok(());
        }
    };

    let autoposter = lock
        .get::<AutoPosterClient>()
        .ok_or(anyhow!("Autoposter client not found"))?;

    match autoposter
        .register_autopost(
            tarpc::context::current(),
            custom_id["subreddit"].to_string().replace('"', ""),
            modal.channel_id,
            interval,
            limit,
            search,
            ctx.http.application_id().expect("app ID missing").get(),
        )
        .await
    {
        Ok(_) => {}
        Err(e) => {
            error!("Failed to send start autopost command: {:?}", e);

            let error_response = CreateInteractionResponseMessage::new()
                .embed(CreateEmbed::new()
                    .title("Error Starting Autopost")
                    .description("An internal error occurred while starting the autopost loop, please try again.")
                    .color(0xff0000)
                )
                .ephemeral(true);

            match modal
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(error_response),
                )
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    error!("Failed to send error response: {:?}", e);
                }
            };
        }
    };

    modal
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Autopost loop started!")
                    .ephemeral(true),
            ),
        )
        .await
        .unwrap_or_else(|e| {
            warn!("Failed to acknowledge autopost modal: {:?}", e);
            return;
        });

    Ok(())
}
