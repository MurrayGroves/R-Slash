use std::collections::HashMap;

use anyhow::{anyhow, bail};
use auto_poster::AutoPosterClient;
use post_subscriber::{Bot, SubscriberClient};
use rslash_types::{InteractionResponse, InteractionResponseMessage};
use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, Context};
use tarpc::context;

use crate::{capture_event, NAMESPACE};

pub async fn unsubscribe(
    ctx: &Context,
    interaction: &ComponentInteraction,
) -> Result<InteractionResponse, anyhow::Error> {
    let subreddit = match &interaction.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values
            .get(0)
            .ok_or(anyhow!("No subreddit selected"))?
            .clone(),
        _ => {
            bail!("Invalid interaction data kind");
        }
    };

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<SubscriberClient>()
        .ok_or(anyhow!("Subscriber client not found"))?;
    let bot = match (&*NAMESPACE).as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    match client
        .delete_subscription(
            context::current(),
            subreddit.clone(),
            interaction.channel_id.get(),
            bot,
        )
        .await?
    {
        Ok(_) => {}
        Err(e) => {
            bail!(e);
        }
    }

    capture_event(
        ctx.data.clone(),
        "unsubscribe_subreddit",
        None,
        Some(HashMap::from([("subreddit", subreddit.clone())])),
        &interaction.user.id.get().to_string(),
    )
    .await;

    Ok(InteractionResponse::Message(InteractionResponseMessage {
        content: Some(format!("Unsubscribed from r/{}", subreddit)),
        ephemeral: true,
        ..Default::default()
    }))
}

pub async fn autopost_cancel(
    ctx: &Context,
    interaction: &ComponentInteraction,
) -> Result<InteractionResponse, anyhow::Error> {
    let id: i64 = match &interaction.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => values
            .get(0)
            .ok_or(anyhow!("No autopost selected"))?
            .clone(),
        _ => {
            bail!("Invalid interaction data kind");
        }
    }
    .parse()?;

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<AutoPosterClient>()
        .ok_or(anyhow!("Auto-poster client not found"))?;

    let autopost = match client.delete_autopost(context::current(), id).await? {
        Ok(x) => x,
        Err(e) => {
            bail!(e);
        }
    };

    let fancy_text = format!(
        "r/{} - {}, every {} up to {} times",
        autopost.subreddit,
        match autopost.search.as_ref() {
            Some(x) => x,
            None => "Any post",
        },
        pretty_duration::pretty_duration(&autopost.interval, None),
        match autopost.limit {
            Some(x) => x.to_string(),
            None => "infinite".to_owned(),
        }
    );

    capture_event(
        ctx.data.clone(),
        "cancel_autopost",
        None,
        None,
        &interaction.user.id.get().to_string(),
    )
    .await;

    Ok(InteractionResponse::Message(InteractionResponseMessage {
        content: Some(format!("Stopped Autopost: {}", fancy_text)),
        ephemeral: true,
        ..Default::default()
    }))
}
