use auto_poster::AutoPosterClient;
use log::{error, trace};
use post_subscriber::{Bot, SubscriberClient};
use serde_json::json;
use serenity::all::{
    CommandDataOptionValue, CreateButton, CreateInputText, CreateModal, CreateSelectMenu,
    CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle,
};
use serenity::model::{guild, Colour};
use tarpc::context;
use tracing::debug;
use tracing::instrument;

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::time::SystemTime;

use serenity::builder::{CreateActionRow, CreateEmbed, CreateEmbedFooter};
use serenity::prelude::*;

use redis::{self};
use redis::{from_redis_value, AsyncCommands};

use serenity::model::application::CommandInteraction;

use anyhow::{anyhow, bail, Result};

use memberships::*;

use post_api::*;
use rslash_types::InteractionResponse;
use rslash_types::{ConfigStruct, InteractionResponseMessage};

use crate::discord::ResponseTracker;
use crate::{capture_event, NAMESPACE};

// Return error interaction response from current function if bot doesn't have permission to send messages in the channel
macro_rules! error_if_no_send_message_perm {
    ($ctx:expr, $channel:expr, $tracker:expr) => {
        let channel = $channel.to_channel(&$ctx.http).await?;
        if let Some(channel) = channel.guild() {
            let id = $ctx.cache.current_user().id.clone();
            if let Ok(perms) = channel.permissions_for_user(&$ctx.cache, id) {
                if !perms.send_messages() {
                    return $tracker
                        .send_response(InteractionResponse::Message(InteractionResponseMessage {
                            embed: Some(
                                CreateEmbed::default()
                                    .title("Permission Error")
                                    .description(
                                        "I don't have permission to send messages in this channel.",
                                    )
                                    .color(0xff0000)
                                    .to_owned(),
                            ),
                            ephemeral: true,
                            ..Default::default()
                        }))
                        .await;
                }
            }
        }
    };
}

macro_rules! error_if_no_premium {
    ($ctx:expr, $user:expr, $tracker:expr) => {
        let tiers = get_user_tiers_from_ctx($ctx, $user).await;
        if !tiers.bronze.active {
            return $tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                    embed: Some(
                        CreateEmbed::default()
                            .title("Premium Feature")
                            .description(
                                "You must have premium in order to use this command.
                    Get it [here](https://ko-fi.com/rslash)",
                            )
                            .color(0xff0000)
                            .to_owned(),
                    ),
                    ..Default::default()
                }))
                .await;
        }
    };
}

#[instrument(skip(command, ctx, tracker))]
pub async fn get_subreddit_cmd<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let data_read = ctx.data.read().await;

    let config = data_read.get::<ConfigStruct>().unwrap();

    let options = &command.data.options;
    debug!("Command Options: {:?}", options);

    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let search_enabled = options.len() > 1;
    capture_event(
        ctx.data.clone(),
        "subreddit_cmd",
        Some(HashMap::from([
            ("subreddit", subreddit.clone()),
            ("button", "false".to_string()),
            ("search_enabled", search_enabled.to_string()),
        ])),
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    if config.nsfw_subreddits.contains(&subreddit) {
        let mut permitted = true;
        if let Some(guild_id) = command.guild_id {
            if let Some(guild) = ctx.cache.guild(guild_id) {
                if let Some(channel) = guild.channels.get(&command.channel_id) {
                    if !channel.nsfw {
                        permitted = false;
                    }
                }
            }
        }

        if !permitted {
            return tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(CreateEmbed::default()
                    .title("NSFW subreddits can only be used in NSFW channels")
                    .description("Discord requires NSFW content to only be sent in NSFW channels, find out how to fix this [here](https://support.discord.com/hc/en-us/articles/115000084051-NSFW-Channels-and-Content)")
                    .color(Colour::from_rgb(255, 0, 0))
                    .to_owned()
                ),
                ..Default::default()
            })).await;
        }
    }

    debug!("Getting redis client");
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();
    debug!("Got redis client");
    if options.len() > 1 {
        let search = options[1].value.as_str().unwrap().to_string();
        return tracker
            .send_response(
                get_subreddit_search(subreddit, search, &mut con, command.channel_id).await?,
            )
            .await;
    } else {
        return tracker
            .send_response(get_subreddit(subreddit, &mut con, command.channel_id).await?)
            .await;
    }
}

#[instrument(skip(command, ctx, tracker))]
pub async fn cmd_get_user_tiers<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let data_lock = ctx.data.read().await;
    let config = data_lock.get::<ConfigStruct>().unwrap();
    let mongodb_client = &mut config.mongodb.clone();

    let tiers = get_user_tiers(command.user.id.get().to_string(), mongodb_client).await;
    debug!("Tiers: {:?}", tiers);

    let bronze = match tiers.bronze.active {
        true => "Active",
        false => "Inactive",
    }
    .to_string();

    capture_event(
        ctx.data.clone(),
        "cmd_get_user_tiers",
        Some(HashMap::from([("bronze_active", bronze.to_string())])),
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            embed: Some(
                CreateEmbed::default()
                    .title("Your membership tiers")
                    .description("Get Premium here: https://ko-fi.com/rslash")
                    .field("Premium", bronze, false)
                    .to_owned(),
            ),
            ephemeral: true,
            ..Default::default()
        }))
        .await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn get_custom_subreddit<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    error_if_no_premium!(ctx, command.user.id, tracker);

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    match check_subreddit_valid(&subreddit).await? {
        SubredditStatus::Valid => {}
        SubredditStatus::Invalid(reason) => {
            debug!("Subreddit response not 200: {}", reason);
            return tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                    embed: Some(
                        CreateEmbed::default()
                            .title("Subreddit Inaccessible")
                            .description(format!("r/{} is private or does not exist.", subreddit))
                            .color(0xff0000)
                            .to_owned(),
                    ),
                    ..Default::default()
                }))
                .await;
        }
    }

    tracker.defer().await?;

    let data_lock = ctx.data.read().await;
    let conf = data_lock.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    let id = ctx.cache.current_user().id.get();

    queue_subreddit(&subreddit, &mut con, id).await?;

    drop(data_lock);

    return get_subreddit_cmd(command, ctx, tracker).await;
}

#[instrument(skip(command, ctx, tracker))]
pub async fn info<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let data_read = ctx.data.read().await;
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    capture_event(
        ctx.data.clone(),
        "cmd_info",
        None,
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    let guild_counts: HashMap<String, redis::Value> = con
        .hgetall(format!("shard_guild_counts_{}", &*NAMESPACE))
        .await?;
    let mut guild_count = 0;
    for (_, count) in guild_counts {
        guild_count += from_redis_value::<u64>(&count)?;
    }

    let id = ctx.cache.current_user().id.get();

    tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
        embed: Some(CreateEmbed::default()
            .title("Info")
            .color(0x00ff00).to_owned()
            .footer(CreateEmbedFooter::new(
                format!("v{} compiled at {}", env!("CARGO_PKG_VERSION"), compile_time::datetime_str!())
            ))
            .fields(vec![
                ("Servers".to_string(), guild_count.to_string(), true),
                ("Shard ID".to_string(), ctx.shard_id.to_string(), true),
                ("Shard Count".to_string(), ctx.cache.shard_count().to_string(), true),
            ]).to_owned()
        ),
        components: Some(vec![CreateActionRow::Buttons(vec![
                CreateButton::new_link("https://discord.gg/jYtCFQG")
                    .label("Get Help"),
                CreateButton::new_link(format!("https://discord.com/api/oauth2/authorize?client_id={}&permissions=515463498752&scope=applications.commands%20bot", id))
                    .label("Add to another server"),
                ]
        ), CreateActionRow::Buttons(vec![
            CreateButton::new_link("https://pastebin.com/DtZvJJhG")
                .label("Privacy Policy"),
            CreateButton::new_link("https://pastebin.com/6c4z3uM5")
                .label("Terms & Conditions"),
            ]
    )]),
        ..Default::default()
    })).await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn subscribe_custom<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    error_if_no_premium!(ctx, command.user.id, tracker);
    subscribe(command, ctx, tracker).await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn subscribe<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<(), anyhow::Error> {
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(CreateEmbed::default()
                    .title("Permission Error")
                    .description("You must have the 'Manage Messages' permission to setup a subscription.")
                    .color(0xff0000).to_owned()
                ),
                ephemeral: true,
                ..Default::default()
            })).await;
        }
    }

    error_if_no_send_message_perm!(ctx, command.channel_id, tracker);

    let options = &command.data.options;
    debug!("Command Options: {:?}", options);

    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<SubscriberClient>()
        .ok_or(anyhow!("Subscriber client not found"))?;

    let bot = match (&*NAMESPACE).as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    if let Err(x) = client
        .register_subscription(
            context::current(),
            subreddit.clone(),
            command.channel_id.get(),
            bot,
        )
        .await
    {
        if format!("{}", x).contains("Already subscribed") {
            return tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                    embed: Some(
                        CreateEmbed::default()
                            .title("Already Subscribed")
                            .description(format!(
                                "This channel is already subscribed to r/{}",
                                subreddit
                            ))
                            .color(0xff0000)
                            .to_owned(),
                    ),
                    ..Default::default()
                }))
                .await;
        } else {
            bail!(x);
        }
    };

    capture_event(
        ctx.data.clone(),
        "subscribe_subreddit",
        Some(HashMap::from([("subreddit", subreddit.clone())])),
        &command.user.id.get().to_string(),
    )
    .await;

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            embed: Some(
                CreateEmbed::default()
                    .title("Subscribed")
                    .description(format!(
                        "This channel has been subscribed to r/{}",
                        subreddit
                    ))
                    .color(0x00ff00)
                    .to_owned(),
            ),
            ..Default::default()
        }))
        .await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn unsubscribe<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(CreateEmbed::default()
                    .title("Permission Error")
                    .description("You must have the 'Manage Messages' permission to manage subscriptions.")
                    .color(0xff0000).to_owned()
                ),
                ephemeral: true,
                ..Default::default()
            })).await;
        }
    }

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<SubscriberClient>()
        .ok_or(anyhow!("Subscriber client not found"))?;

    let bot = match (&*NAMESPACE).as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    debug!("Deadline: {:?}", context::current().deadline);
    debug!("Now: {:?}", SystemTime::now());
    let subreddits = match client
        .list_subscriptions(context::current(), command.channel_id.get(), bot)
        .await?
    {
        Ok(x) => x,
        Err(_) => {
            return Err(anyhow!("Error getting subscriptions"));
        }
    };

    if subreddits.len() == 0 {
        return tracker
            .send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(
                    CreateEmbed::default()
                        .title("No Subscriptions")
                        .description("This channel has no subscriptions.")
                        .color(0xff0000)
                        .to_owned(),
                ),
                ephemeral: true,
                ..Default::default()
            }))
            .await;
    }

    let menu = CreateSelectMenu::new(
        json!({"command": "unsubscribe"}).to_string(),
        CreateSelectMenuKind::String {
            options: subreddits
                .into_iter()
                .map(|x| CreateSelectMenuOption::new("r/".to_owned() + &x.subreddit, x.subreddit))
                .collect(),
        },
    )
    .placeholder("Select a subreddit to unsubscribe from")
    .min_values(1)
    .max_values(1);

    let components = vec![CreateActionRow::SelectMenu(menu)];

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            ephemeral: true,
            components: Some(components),
            ..Default::default()
        }))
        .await
}

#[instrument(skip(command, ctx, tracker))]
async fn autopost_start<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    subreddit: String,
    search: Option<&str>,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    if let Some(member) = &command.member {
        let permissions = match member.permissions(&ctx) {
            Ok(x) => x,
            Err(e) => {
                if let Some(guild_id) = command.guild_id {
                    let member = guild_id.member(&ctx, command.user.id).await?;
                    member.permissions(&ctx)?
                } else {
                    return Err(anyhow!("Error getting permissions: {:?}", e));
                }
            }
        };
        if !permissions.manage_channels() {
            return tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(
                    CreateEmbed::default()
                        .title("Permission Error")
                        .description(
                            "You must have the 'Manage Channels' permission to setup auto-post.",
                        )
                        .color(0xff0000)
                        .to_owned(),
                ),
                ephemeral: true,
                ..Default::default()
            })).await;
        }
    }

    error_if_no_send_message_perm!(ctx, command.channel_id, tracker);

    let is_premium = get_user_tiers_from_ctx(ctx, command.user.id)
        .await
        .bronze
        .active;

    let max_length = match is_premium {
        true => 100,
        false => 2,
    };

    let components = vec![
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Delay", "delay")
                .label("Delay e.g. 5s, 3m, 5h, 1d")
                .placeholder("5s")
                .min_length(2)
                .max_length(6),
        ),
        CreateActionRow::InputText(
            CreateInputText::new(InputTextStyle::Short, "Limit", "limit")
                .label("Times to post before stopping e.g. 10")
                .placeholder("Can be \"infinite\" if you have premium")
                .min_length(1)
                .max_length(max_length),
        ),
    ];

    tracker
        .send_response(InteractionResponse::Modal(
            CreateModal::new(
                serde_json::to_string(&json!({
                    "subreddit": subreddit,
                    "command": "autopost",
                    "search": search
                }))
                .unwrap(),
                "Autopost Setup",
            )
            .components(components),
        ))
        .await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn autopost_stop<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    trace!("autopost_stop command handler");
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(
                    CreateEmbed::default()
                        .title("Permission Error")
                        .description(
                            "You must have the 'Manage Messages' permission to manage autoposts.",
                        )
                        .color(0xff0000)
                        .to_owned(),
                ),
                ..Default::default()
            })).await;
        }
    }

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<AutoPosterClient>()
        .ok_or(anyhow!("Auto-poster client not found"))?;

    let bot = ctx.http.application_id().unwrap().get();

    let autoposts = match match client
        .list_autoposts(context::current(), command.channel_id.get(), bot)
        .await
    {
        Ok(x) => x,
        Err(e) => {
            error!("Error calling list_autoposts: {:?}\n{:?}", e, e.source());
            return Err(anyhow!("Error getting autoposts"));
        }
    } {
        Ok(x) => x,
        Err(_) => {
            return Err(anyhow!("Error getting autoposts"));
        }
    };

    if autoposts.len() == 0 {
        return tracker
            .send_response(InteractionResponse::Message(InteractionResponseMessage {
                embed: Some(
                    CreateEmbed::default()
                        .title("No Autoposts")
                        .description("This channel has no autoposts running.")
                        .color(0xff0000)
                        .to_owned(),
                ),
                ephemeral: true,
                ..Default::default()
            }))
            .await;
    }

    let mut fancy_texts = Vec::new();
    for autopost in autoposts.iter() {
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
        fancy_texts.push((fancy_text, autopost.id));
    }
    let menu = CreateSelectMenu::new(
        json!({"command": "autopost_cancel"}).to_string(),
        CreateSelectMenuKind::String {
            options: fancy_texts
                .into_iter()
                .map(|x| CreateSelectMenuOption::new(x.0, x.1.to_string()))
                .collect(),
        },
    )
    .placeholder("Select a subreddit to unsubscribe from")
    .min_values(1)
    .max_values(1);

    let components = vec![CreateActionRow::SelectMenu(menu)];

    tracker
        .send_response(InteractionResponse::Message(InteractionResponseMessage {
            ephemeral: true,
            components: Some(components),
            ..Default::default()
        }))
        .await
}

#[instrument(skip(command, ctx, tracker))]
pub async fn autopost<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let options = &command.data.options;
    if let Some(first) = options.get(0) {
        if first.name == "start" {
            if let CommandDataOptionValue::SubCommand(options) = &first.value {
                let subreddit = options[0].value.clone();
                let subreddit = subreddit
                    .as_str()
                    .ok_or(anyhow!("Subreddit parameter couldn't be string"))?
                    .to_string()
                    .to_lowercase();
                let search = match options.get(1) {
                    Some(x) => x.value.clone(),
                    None => CommandDataOptionValue::Number(0 as f64),
                };
                autopost_start(command, ctx, subreddit, search.as_str(), tracker).await
            } else {
                Err(anyhow!("Invalid subcommand"))
            }
        } else if first.name == "stop" {
            autopost_stop(command, ctx, tracker).await
        } else if first.name == "custom" {
            if let CommandDataOptionValue::SubCommand(options) = &first.value {
                error_if_no_premium!(ctx, command.user.id, tracker);

                let subreddit = options[0].value.clone();
                let subreddit = subreddit
                    .as_str()
                    .ok_or(anyhow!("Subreddit parameter couldn't be string"))?
                    .to_string()
                    .to_lowercase();
                let search = match options.get(1) {
                    Some(x) => x.value.clone(),
                    None => CommandDataOptionValue::Number(0 as f64),
                };
                autopost_start(command, ctx, subreddit, search.as_str(), tracker).await
            } else {
                Err(anyhow!("Invalid subcommand"))
            }
        } else {
            Err(anyhow!("Invalid subcommand"))
        }
    } else {
        Err(anyhow!("No subcommand"))
    }
}
