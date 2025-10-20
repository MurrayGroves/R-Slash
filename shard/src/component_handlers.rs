use std::{collections::HashMap, time::Duration};

use anyhow::{Result, anyhow, bail};
use post_api::{get_subreddit, get_subreddit_search, optional_post_to_response};
use post_subscriber::Bot;
use serenity::all::{
    ComponentInteraction, ComponentInteractionDataKind, Context, CreateInteractionResponse,
    CreateInteractionResponseMessage,
};
use tarpc::context;
use tokio::time::timeout;
use tracing::{debug, instrument};

use crate::{NAMESPACE, ShardState, capture_event, discord::ResponseTracker};

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

    let client = ctx.data::<ShardState>().post_subscriber.clone();

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
        ctx.data(),
        "unsubscribe_subreddit",
        Some(HashMap::from([("subreddit", subreddit.clone())])),
        interaction.guild_id,
        Some(interaction.channel_id),
        &format!("user_{}", &interaction.user.id.get().to_string()),
    )
    .await;

    tracker
        .send_response(CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::default()
                .content(format!("Unsubscribed from r/{}", subreddit))
                .ephemeral(true),
        ))
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

    let client = ctx.data::<ShardState>().auto_poster.clone();

    let autopost = match client
        .delete_autopost(context::current(), id, interaction.channel_id.get())
        .await?
    {
        Ok(x) => x,
        Err(e) => {
            bail!(e);
        }
    };

    let autopost = if let Some(autopost) = autopost {
        autopost
    } else {
        return tracker
            .send_response(CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::default()
                    .content("You already stopped this autopost.")
                    .ephemeral(true),
            ))
            .await;
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
        ctx.data(),
        "cancel_autopost",
        None,
        interaction.guild_id,
        Some(interaction.channel_id),
        &format!("user_{}", interaction.user.id.get().to_string()),
    )
    .await;

    tracker
        .send_response(CreateInteractionResponse::Message(
            CreateInteractionResponseMessage::default()
                .content(format!("Stopped Autopost: {}", fancy_text))
                .ephemeral(true),
        ))
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
        ctx.data(),
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

    let data = ctx.data::<ShardState>();
    let mut con = data.redis.clone();
    let mut mongodb = data.mongodb.clone();

    let component_response = match search_enabled {
        true => {
            let search = custom_data["search"].to_string().replace('"', "");
            timeout(
                Duration::from_secs(30),
                get_subreddit_search(&subreddit, &search, &mut con, interaction.channel_id),
            )
            .await
            .unwrap_or_else(|x| Err(anyhow!("Timeout getting search results: {:?}", x)))
            .map(|post| optional_post_to_response(post))
        }
        false => timeout(
            Duration::from_secs(30),
            get_subreddit(
                &subreddit,
                &mut con,
                &mut mongodb,
                interaction.channel_id,
                Some(0),
            ),
        )
        .await
        .unwrap_or_else(|x| Err(anyhow!("Timeout getting subreddit: {:?}", x)))
        .map(|post| post.into()),
    };

    tracker.send_response(component_response?).await
}
