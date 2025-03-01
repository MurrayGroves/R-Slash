use std::{collections::HashMap, time::Duration};

use anyhow::{Result, anyhow, bail};
use auto_poster::AutoPosterClient;
use post_api::{get_subreddit, get_subreddit_search};
use post_subscriber::{Bot, SubscriberClient};
use rslash_common::{ConfigStruct, InteractionResponse, InteractionResponseMessage};
use serenity::all::{ComponentInteraction, ComponentInteractionDataKind, Context};
use tarpc::context;
use tokio::time::timeout;
use tracing::{debug, instrument};

use crate::{NAMESPACE, capture_event, discord::ResponseTracker};

#[instrument(skip(ctx, interaction, tracker))]
pub async fn unsubscribe<'a>(
    ctx: &Context,
    interaction: &ComponentInteraction,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
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
        Some(HashMap::from([("subreddit", subreddit.clone())])),
        interaction.guild_id,
        Some(interaction.channel_id),
        &format!("user_{}", &interaction.user.id.get().to_string()),
    )
    .await;

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            content: Some(format!("Unsubscribed from r/{}", subreddit)),
            ephemeral: true,
            ..Default::default()
        }))
        .await
}

#[instrument(skip(ctx, interaction, tracker))]
pub async fn autopost_cancel<'a>(
    ctx: &Context,
    interaction: &ComponentInteraction,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
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

    let autopost = match client.delete_autopost(context::current(), id, interaction.channel_id.get()).await? {
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
        interaction.guild_id,
        Some(interaction.channel_id),
        &format!("user_{}", interaction.user.id.get().to_string()),
    )
    .await;

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            content: Some(format!("Stopped Autopost: {}", fancy_text)),
            ephemeral: true,
            ..Default::default()
        }))
        .await
}

#[instrument(skip(ctx, interaction, tracker))]
pub async fn post_again<'a>(
    ctx: &Context,
    interaction: &ComponentInteraction,
    custom_data: HashMap<String, serde_json::Value>,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let subreddit = custom_data["subreddit"].to_string().replace('"', "");
    debug!("Search, {:?}", custom_data["search"]);
    let search_enabled = match custom_data["search"] {
        serde_json::Value::String(_) => true,
        _ => false,
    };

    capture_event(
        ctx.data.clone(),
        "subreddit_cmd",
        Some(HashMap::from([
            ("subreddit", subreddit.clone().to_lowercase()),
            ("button", "true".to_string()),
            ("search_enabled", search_enabled.to_string()),
        ])),
        interaction.guild_id,
        Some(interaction.channel_id),
        &format!("user_{}", interaction.user.id.get().to_string()),
    )
    .await;

    let data_read = ctx.data.read().await;
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    let component_response = match search_enabled {
        true => {
            let search = custom_data["search"].to_string().replace('"', "");
            match timeout(
                Duration::from_secs(30),
                get_subreddit_search(subreddit, search, &mut con, interaction.channel_id),
            )
            .await
            {
                Ok(x) => x,
                Err(x) => Err(anyhow!("Timeout getting search results: {:?}", x)),
            }
        }
        false => {
            match timeout(
                Duration::from_secs(30),
                get_subreddit(subreddit, &mut con, interaction.channel_id),
            )
            .await
            {
                Ok(x) => x,
                Err(x) => Err(anyhow!("Timeout getting subreddit: {:?}", x)),
            }
        }
    };

    tracker.send_response(component_response?).await
}
