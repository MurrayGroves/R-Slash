use std::collections::HashMap;

use anyhow::{anyhow, bail};
use post_subscriber::{Bot, SubscriberClient};
use rslash_types::InteractionResponse;
use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, ComponentType, Context};
use tarpc::context;

use crate::{capture_event, get_namespace};

pub async fn unsubscribe(ctx: &Context, interaction: &ComponentInteraction) -> Result<InteractionResponse, anyhow::Error> {
    let subreddit = match &interaction.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => {
            values.get(0).ok_or(anyhow!("No subreddit selected"))?.clone()
        },
        _ => {
            bail!("Invalid interaction data kind");
        }
    };

    let data_lock = ctx.data.read().await;
    let client = data_lock.get::<SubscriberClient>().ok_or(anyhow!("Subscriber client not found"))?.clone();

    let bot = match get_namespace().as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    match client.delete_subscription(context::current(), subreddit.clone(), interaction.channel_id.get(), bot).await? {
        Ok(_) => {},
        Err(e) => {
            bail!(e);
        }
    }

    capture_event(ctx.data.clone(), "unsubscribe_subreddit", None, Some(HashMap::from([("subreddit", subreddit.clone())])), &interaction.user.id.get().to_string()).await;

    Ok(InteractionResponse {
        content: Some(format!("Unsubscribed from r/{}", subreddit)),
        ephemeral: true,
        ..Default::default()
    })
}

