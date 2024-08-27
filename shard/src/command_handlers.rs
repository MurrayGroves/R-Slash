use auto_poster::AutoPosterClient;
use log::{error, trace};
use post_subscriber::{Bot, SubscriberClient};
use serde_json::json;
use serenity::all::{
    CommandDataOptionValue, CreateButton, CreateInputText, CreateInteractionResponse,
    CreateInteractionResponseMessage, CreateModal, CreateSelectMenu, CreateSelectMenuKind,
    CreateSelectMenuOption, InputTextStyle,
};
use serenity::model::Colour;
use tarpc::context;
use tracing::instrument;
use tracing::{debug, warn};

use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::time::SystemTime;

use serenity::builder::{CreateActionRow, CreateEmbed, CreateEmbedFooter};
use serenity::prelude::*;

use tokio::time::{sleep, Duration};

use redis::{self};
use redis::{from_redis_value, AsyncCommands};

use serenity::model::application::CommandInteraction;

use anyhow::{anyhow, bail, Result};

use connection_pooler::ResourceManager;
use memberships::*;

use post_api::*;
use rslash_types::ConfigStruct;
use rslash_types::InteractionResponse;

use crate::{capture_event, get_epoch_ms, get_namespace};

#[instrument(skip(command, ctx, tx))]
pub async fn get_subreddit_cmd<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
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
        Some(tx),
        Some(HashMap::from([
            ("subreddit", subreddit.clone()),
            ("button", "false".to_string()),
            ("search_enabled", search_enabled.to_string()),
        ])),
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    if config.nsfw_subreddits.contains(&subreddit) {
        if let Some(channel) = command.channel_id.to_channel_cached(&ctx.cache) {
            if !channel.nsfw {
                return Ok(InteractionResponse {
                    embed: Some(CreateEmbed::default()
                        .title("NSFW subreddits can only be used in NSFW channels")
                        .description("Discord requires NSFW content to only be sent in NSFW channels, find out how to fix this [here](https://support.discord.com/hc/en-us/articles/115000084051-NSFW-Channels-and-Content)")
                        .color(Colour::from_rgb(255, 0, 0))
                        .to_owned()
                    ),
                    ..Default::default()
                });
            }
        }
    }

    debug!("Getting redis client");
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();
    debug!("Got redis client");
    if options.len() > 1 {
        let search = options[1].value.as_str().unwrap().to_string();
        return get_subreddit_search(subreddit, search, &mut con, command.channel_id, Some(tx))
            .await;
    } else {
        return get_subreddit(subreddit, &mut con, command.channel_id, Some(tx)).await;
    }
}

#[instrument(skip(command, ctx, parent_tx))]
pub async fn cmd_get_user_tiers<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    parent_tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    let data_lock = ctx.data.read().await;
    let mongodb_manager = data_lock
        .get::<ResourceManager<mongodb::Client>>()
        .ok_or(anyhow!("Mongodb client manager not found"))?
        .clone();
    let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
    let mut mongodb_client = mongodb_client_mutex.lock().await;

    let tiers = get_user_tiers(
        command.user.id.get().to_string(),
        &mut *mongodb_client,
        Some(parent_tx),
    )
    .await;
    debug!("Tiers: {:?}", tiers);

    let bronze = match tiers.bronze.active {
        true => "Active",
        false => "Inactive",
    }
    .to_string();

    capture_event(
        ctx.data.clone(),
        "cmd_get_user_tiers",
        Some(parent_tx),
        Some(HashMap::from([("bronze_active", bronze.to_string())])),
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    return Ok(InteractionResponse {
        embed: Some(
            CreateEmbed::default()
                .title("Your membership tiers")
                .description("Get Premium here: https://ko-fi.com/rslash")
                .field("Premium", bronze, false)
                .to_owned(),
        ),
        ..Default::default()
    });
}

#[instrument(skip(command, ctx, parent_tx))]
pub async fn get_custom_subreddit<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    parent_tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    let data_lock = ctx.data.read().await;
    let mongodb_manager = data_lock
        .get::<ResourceManager<mongodb::Client>>()
        .ok_or(anyhow!("Mongodb client manager not found"))?
        .clone();
    let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
    let mut mongodb_client = mongodb_client_mutex.lock().await;

    let membership = get_user_tiers(
        command.user.id.get().to_string(),
        &mut *mongodb_client,
        Some(parent_tx),
    )
    .await;
    if !membership.bronze.active {
        return Ok(InteractionResponse {
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
        });
    }

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let web_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!(
            "Discord:RSlash:{} (by /u/murrax2)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let res = web_client
        .head(format!("https://www.reddit.com/r/{}.json", subreddit))
        .send()
        .await?;

    if res.status() != 200 {
        debug!("Subreddit response not 200: {}", res.text().await?);
        return Ok(InteractionResponse {
            embed: Some(
                CreateEmbed::default()
                    .title("Subreddit Inaccessible")
                    .description(format!("r/{} is private or does not exist.", subreddit))
                    .color(0xff0000)
                    .to_owned(),
            ),
            ..Default::default()
        });
    }

    let conf = data_lock.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    let already_queued = list_contains(
        &subreddit,
        "custom_subreddits_queue",
        &mut con,
        Some(parent_tx),
    )
    .await?;

    let last_cached: i64 = con.get(&format!("{}", subreddit)).await.unwrap_or(0);

    if last_cached == 0 {
        debug!("Subreddit not cached");

        command.defer(&ctx.http).await.unwrap_or_else(|e| {
            warn!("Failed to defer response: {}", e);
        });
        if !already_queued {
            debug!("Queueing subreddit for download");
            con.rpush("custom_subreddits_queue", &subreddit).await?;

            let selector = if ctx.cache.current_user().id.get() == 278550142356029441 {
                "nsfw"
            } else {
                "sfw"
            };

            con.hset(
                &format!("custom_sub:{}:{}", selector, subreddit),
                "name",
                &subreddit,
            )
            .await?;
        }
        loop {
            sleep(Duration::from_millis(50)).await;

            let posts: Vec<String> = match redis::cmd("LRANGE")
                .arg(format!("subreddit:{}:posts", subreddit.clone()))
                .arg(0i64)
                .arg(0i64)
                .query_async(&mut con)
                .await
            {
                Ok(posts) => posts,
                Err(_) => {
                    continue;
                }
            };
            if posts.len() > 0 {
                break;
            }
        }
    } else if last_cached + 3600000 < get_epoch_ms() as i64 {
        debug!("Subreddit last cached more than an hour ago, updating...");
        // Tell downloader to update the subreddit, but use outdated posts for now.
        if !already_queued {
            con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
    }

    return get_subreddit_cmd(command, ctx, parent_tx).await;
}

#[instrument(skip(command, ctx, parent_tx))]
pub async fn info<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    parent_tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    let data_read = ctx.data.read().await;
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    capture_event(
        ctx.data.clone(),
        "cmd_info",
        Some(parent_tx),
        None,
        &format!("user_{}", command.user.id.get().to_string()),
    )
    .await;

    let guild_counts: HashMap<String, redis::Value> = con
        .hgetall(format!("shard_guild_counts_{}", get_namespace()))
        .await?;
    let mut guild_count = 0;
    for (_, count) in guild_counts {
        guild_count += from_redis_value::<u64>(&count)?;
    }

    let id = ctx.cache.current_user().id.get();

    return Ok(InteractionResponse {
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
    });
}

pub async fn subscribe_custom<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    parent_tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    {
        let data_lock = ctx.data.read().await;
        let mongodb_manager = data_lock
            .get::<ResourceManager<mongodb::Client>>()
            .ok_or(anyhow!("Mongodb client manager not found"))?
            .clone();
        let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
        let mut mongodb_client = mongodb_client_mutex.lock().await;

        let membership = get_user_tiers(
            command.user.id.get().to_string(),
            &mut *mongodb_client,
            Some(parent_tx),
        )
        .await;
        if !membership.bronze.active {
            return Ok(InteractionResponse {
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
            });
        }
    }
    subscribe(command, ctx, parent_tx).await
}

pub async fn subscribe<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    parent_tx: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return Ok(InteractionResponse {
                embed: Some(CreateEmbed::default()
                    .title("Permission Error")
                    .description("You must have the 'Manage Messages' permission to setup a subscription.")
                    .color(0xff0000).to_owned()
                ),
                ..Default::default()
            });
        }
    }

    let options = &command.data.options;
    debug!("Command Options: {:?}", options);

    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<SubscriberClient>()
        .ok_or(anyhow!("Subscriber client not found"))?
        .clone();

    let bot = match get_namespace().as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    let tx = parent_tx.start_child("subscribe_call", "subscribe");
    if let Err(x) = client
        .register_subscription(
            context::current(),
            subreddit.clone(),
            command.channel_id.get(),
            bot,
        )
        .await
    {
        tx.finish();
        if format!("{}", x).contains("Already subscribed") {
            return Ok(InteractionResponse {
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
            });
        } else {
            bail!(x);
        }
    };
    tx.finish();

    capture_event(
        ctx.data.clone(),
        "subscribe_subreddit",
        Some(parent_tx),
        Some(HashMap::from([("subreddit", subreddit.clone())])),
        &command.user.id.get().to_string(),
    )
    .await;

    return Ok(InteractionResponse {
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
    });
}

pub async fn unsubscribe<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    _: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return Ok(InteractionResponse {
                embed: Some(CreateEmbed::default()
                    .title("Permission Error")
                    .description("You must have the 'Manage Messages' permission to manage subscriptions.")
                    .color(0xff0000).to_owned()
                ),
                ..Default::default()
            });
        }
    }

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<SubscriberClient>()
        .ok_or(anyhow!("Subscriber client not found"))?
        .clone();

    let bot = match get_namespace().as_str() {
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
        return Ok(InteractionResponse {
            embed: Some(
                CreateEmbed::default()
                    .title("No Subscriptions")
                    .description("This channel has no subscriptions.")
                    .color(0xff0000)
                    .to_owned(),
            ),
            ephemeral: true,
            ..Default::default()
        });
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

    return Ok(InteractionResponse {
        ephemeral: true,
        components: Some(components),
        ..Default::default()
    });
}

async fn autopost_start<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    subreddit: String,
    search: Option<&str>,
) -> Result<InteractionResponse> {
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_channels() {
            return Ok(InteractionResponse {
                embed: Some(
                    CreateEmbed::default()
                        .title("Permission Error")
                        .description(
                            "You must have the 'Manage Channels' permission to setup auto-post.",
                        )
                        .color(0xff0000)
                        .to_owned(),
                ),
                ..Default::default()
            });
        }
    }

    let is_premium = {
        let data_read = ctx.data.read().await;
        let mongodb_manager = data_read
            .get::<ResourceManager<mongodb::Client>>()
            .unwrap()
            .clone();
        let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
        let mut mongodb_client = mongodb_client_mutex.lock().await;
        let membership = get_user_tiers(
            command.user.id.get().to_string(),
            &mut *mongodb_client,
            None,
        )
        .await;
        membership.bronze.active
    };

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

    let resp = CreateInteractionResponse::Modal(
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
    );
    match command.create_response(&ctx.http, resp).await {
        Ok(_) => {}
        Err(x) => {
            warn!("Error sending modal: {:?}", x);
        }
    };
    Ok(InteractionResponse::default())
}

pub async fn autopost_stop<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
) -> Result<InteractionResponse, anyhow::Error> {
    trace!("autopost_stop command handler");
    if let Some(member) = &command.member {
        if !member.permissions(&ctx).unwrap().manage_messages() {
            return Ok(InteractionResponse {
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
            });
        }
    }

    let data_lock = ctx.data.read().await;
    let client = data_lock
        .get::<AutoPosterClient>()
        .ok_or(anyhow!("Auto-poster client not found"))?
        .clone();

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
        return Ok(InteractionResponse {
            embed: Some(
                CreateEmbed::default()
                    .title("No Autoposts")
                    .description("This channel has no autoposts running.")
                    .color(0xff0000)
                    .to_owned(),
            ),
            ephemeral: true,
            ..Default::default()
        });
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

    trace!("Sending select menu");
    return Ok(InteractionResponse {
        ephemeral: true,
        components: Some(components),
        ..Default::default()
    });
}

pub async fn autopost<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    _: &sentry::TransactionOrSpan,
) -> Result<InteractionResponse, anyhow::Error> {
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
                autopost_start(command, ctx, subreddit, search.as_str()).await
            } else {
                Err(anyhow!("Invalid subcommand"))
            }
        } else if first.name == "stop" {
            autopost_stop(command, ctx).await
        } else {
            Err(anyhow!("Invalid subcommand"))
        }
    } else {
        Err(anyhow!("No subcommand"))
    }
}
