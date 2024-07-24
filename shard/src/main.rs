use log::trace;
use post_subscriber::{Bot, SubscriberClient};
use serenity::all::{ActionRowComponent, AutocompleteChoice, ButtonStyle, CreateAutocompleteResponse, CreateButton, CreateSelectMenu, CreateSelectMenuKind, CreateSelectMenuOption, InputTextStyle};
use serenity::gateway::ShardStageUpdateEvent;
use serenity::model::Colour;
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::context;
use tarpc::serde_transport::Transport;
use tarpc::tokio_serde::formats::Bincode;
use tracing::{debug, info, warn, error};
use rand::Rng;
use serde_json::json;
use tracing::instrument;
use tracing_subscriber::{Layer, util::SubscriberInitExt, prelude::__tracing_subscriber_SubscriberExt};

use rslash_types::ResponseFallbackMethod;
use std::{fs, iter};
use std::io::Write;
use std::collections::HashMap;
use std::env;

use serenity::builder::{CreateActionRow, CreateEmbed, CreateEmbedFooter, CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal, EditInteractionResponse};
use serenity::model::id::ShardId;
use serenity::model::gateway::GatewayIntents;
use serenity::{
    async_trait,
    model::gateway::Ready,
    prelude::*,
};

use tokio::time::{sleep, timeout, Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use std::fs::File;
use std::path::Path;
use futures_util::TryStreamExt;
use tokio::sync::Mutex;

use redis::{self, FromRedisValue};
use redis::{AsyncCommands, from_redis_value};

use mongodb::options::ClientOptions;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;

use serenity::model::guild::{Guild, UnavailableGuild};
use serenity::model::application::{Interaction, CommandInteraction};

use anyhow::{anyhow, Error};

use memberships::*;
use connection_pooler::ResourceManager;

mod poster;
mod component_handlers;

use rslash_types::AutoPostCommand;
use rslash_types::ConfigStruct;
use rslash_types::InteractionResponse;
use post_api::*;


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn redis_sanitise(input: &str) -> String {
    let special = vec![
        ",", ".", "<", ">", "{", "}", "[", "]", "\"", "'", ":", ";", "!", "@", "#", "$", "%", "^", "&", "*", "(", ")", "-", "+", "=", "~"
    ];

    let mut output = input.to_string();
    for s in special {
        output = output.replace(s, &("\\".to_owned() + s));
    }

    output
}

#[instrument(skip(data, parent_tx))]
async fn capture_event(data: Arc<RwLock<TypeMap>>, event: &str, parent_tx: Option<&sentry::TransactionOrSpan>, properties: Option<HashMap<&str, String>>, distinct_id: &str) {
    let span: sentry::TransactionOrSpan = match parent_tx {
        Some(parent) => parent.start_child("analytics", "capture_event").into(),
        None => {
            let ctx = sentry::TransactionContext::new("analytics", "capture_event");
            sentry::start_transaction(ctx).into()
        }
    };

    let event = event.to_string();
    let properties = match properties {
        Some(properties) => Some(properties.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<HashMap<String, String>>()),
        None => None,
    };
    let distinct_id = distinct_id.to_string();

    tokio::spawn(async move {
        debug!("Getting posthog manager");
        let posthog_manager = data.read().await.get::<ResourceManager<posthog::Client>>().unwrap().clone();
        debug!("Getting posthog client");
        let client_mutex = posthog_manager.get_available_resource().await;
        debug!("Locking client mutex");
        let client = client_mutex.lock().await;
        debug!("Locked client mutex");
    
        let mut properties_map = serde_json::Map::new();
        if properties.is_some() {
            for (key, value) in properties.unwrap() {
                properties_map.insert(key.to_string(), serde_json::Value::String(value));
            }
        }

        match client.capture(&event, properties_map, &distinct_id).await {
            Ok(_) => {},
            Err(e) => {
                warn!("Error capturing event: {:?}", e);
            }
        }    
    });

    span.finish();
}


#[instrument(skip(command, ctx, tx))]
async fn get_subreddit_cmd<'a>(command: &'a CommandInteraction, ctx: &'a Context, tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
    let data_read = ctx.data.read().await;

    let config = data_read.get::<ConfigStruct>().unwrap();

    let options = &command.data.options;
    debug!("Command Options: {:?}", options);

    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let search_enabled = options.len() > 1;
    capture_event(ctx.data.clone(), "subreddit_cmd", Some(tx),
                    Some(HashMap::from([("subreddit", subreddit.clone()), ("button", "false".to_string()), ("search_enabled", search_enabled.to_string())])), &format!("user_{}", command.user.id.get().to_string())
                ).await;

    if config.nsfw_subreddits.contains(&subreddit) {
        if let Some(channel) = command.channel_id.to_channel_cached(&ctx.cache) {
            if !channel.is_nsfw() {
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
        return get_subreddit_search(subreddit, search, &mut con, command.channel_id, Some(tx)).await
    }
    else {
        return get_subreddit(subreddit, &mut con, command.channel_id, Some(tx)).await
    }
}


fn error_response(code: String) -> InteractionResponse {
    let embed = CreateEmbed::default()
        .title("An Error Occurred")
        .description(format!("Please report this in the support server.\n Error: {}", code))
        .color(Colour::from_rgb(255, 0, 0)).to_owned();
        
    return InteractionResponse {
        file: None,
        embed: Some(embed),
        content: None,
        ephemeral: true,
        components: None,
        fallback: ResponseFallbackMethod::Followup,
    }
}


#[instrument(skip(command, ctx, parent_tx))]
async fn cmd_get_user_tiers<'a>(command: &'a CommandInteraction, ctx: &'a Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
    let data_lock = ctx.data.read().await;
    let mongodb_manager = data_lock.get::<ResourceManager<mongodb::Client>>().ok_or(anyhow!("Mongodb client manager not found"))?.clone();
    let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
    let mut mongodb_client = mongodb_client_mutex.lock().await;

    let tiers = get_user_tiers(command.user.id.get().to_string(), &mut *mongodb_client, Some(parent_tx)).await;
    debug!("Tiers: {:?}", tiers);

    let bronze = match tiers.bronze.active {
        true => "Active",
        false => "Inactive",
    }.to_string();


    capture_event(ctx.data.clone(), "cmd_get_user_tiers", Some(parent_tx), Some(HashMap::from([("bronze_active", bronze.to_string())])), &format!("user_{}", command.user.id.get().to_string())).await;

    return Ok(InteractionResponse {
        embed: Some(CreateEmbed::default()
            .title("Your membership tiers")
            .description("Get Premium here: https://ko-fi.com/rslash")
            .field("Premium", bronze, false).to_owned()
        ),
        ..Default::default()
    });
}


#[instrument(skip(command, ctx, parent_tx))]
async fn get_custom_subreddit<'a>(command: &'a CommandInteraction, ctx: &'a Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
    let data_lock = ctx.data.read().await;
    let mongodb_manager = data_lock.get::<ResourceManager<mongodb::Client>>().ok_or(anyhow!("Mongodb client manager not found"))?.clone();
    let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
    let mut mongodb_client = mongodb_client_mutex.lock().await;


    let membership = get_user_tiers(command.user.id.get().to_string(), &mut *mongodb_client, Some(parent_tx)).await;
    if !membership.bronze.active {
        return Ok(InteractionResponse {
            embed: Some(CreateEmbed::default()
                .title("Premium Feature")
                .description("You must have premium in order to use this command.
                Get it [here](https://ko-fi.com/rslash)")
                .color(0xff0000).to_owned()
            ),
            ..Default::default()
        });
    }

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.as_str().unwrap().to_string().to_lowercase();

    let web_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(format!("Discord:RSlash:{} (by /u/murrax2)", env!("CARGO_PKG_VERSION")))
        .build()?;
    let res = web_client
        .head(format!("https://www.reddit.com/r/{}.json", subreddit))
        .send()
        .await?;

    if res.status() != 200 {
        debug!("Subreddit response not 200: {}", res.text().await?);
        return Ok(InteractionResponse {
            embed: Some(CreateEmbed::default()
                .title("Subreddit Inaccessible")
                .description(format!("r/{} is private or does not exist.", subreddit))
                .color(0xff0000).to_owned()
            ),
            ..Default::default()
        });
    }

    let conf = data_lock.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    let already_queued = list_contains(&subreddit, "custom_subreddits_queue", &mut con, Some(parent_tx)).await?;

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

            con.hset(&format!("custom_sub:{}:{}", selector, subreddit), "name", &subreddit).await?;
        }
        loop {
            sleep(Duration::from_millis(50)).await;

            let posts: Vec<String> = match redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(0i64).query_async(&mut con).await {
                Ok(posts) => {
                    posts
                },
                Err(_) => {
                    continue;
                }
            };
            if posts.len() > 0 {
                break;
            }
        }
    } else if last_cached +  3600000 < get_epoch_ms() as i64 {
        debug!("Subreddit last cached more than an hour ago, updating...");
        // Tell downloader to update the subreddit, but use outdated posts for now.
        if !already_queued {
            con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
    }

    return get_subreddit_cmd(command, ctx, parent_tx).await;
}

#[instrument(skip(command, ctx, parent_tx))]
async fn info<'a>(command: &'a CommandInteraction, ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
    let data_read = ctx.data.read().await;    
    let conf = data_read.get::<ConfigStruct>().unwrap();
    let mut con = conf.redis.clone();

    capture_event(ctx.data.clone(), "cmd_info", Some(parent_tx), None, &format!("user_{}", command.user.id.get().to_string())).await;

    let guild_counts: HashMap<String, redis::Value> = con.hgetall(format!("shard_guild_counts_{}", get_namespace())).await?;
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
    })
}


async fn subscribe_custom<'a>(command: &'a CommandInteraction, ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
    {
        let data_lock = ctx.data.read().await;
        let mongodb_manager = data_lock.get::<ResourceManager<mongodb::Client>>().ok_or(anyhow!("Mongodb client manager not found"))?.clone();
        let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
        let mut mongodb_client = mongodb_client_mutex.lock().await;


        let membership = get_user_tiers(command.user.id.get().to_string(), &mut *mongodb_client, Some(parent_tx)).await;
        if !membership.bronze.active {
            return Ok(InteractionResponse {
                embed: Some(CreateEmbed::default()
                    .title("Premium Feature")
                    .description("You must have premium in order to use this command.
                    Get it [here](https://ko-fi.com/rslash)")
                    .color(0xff0000).to_owned()
                ),
                ..Default::default()
            });
        }
    }
    subscribe(command, ctx, parent_tx).await
}

async fn subscribe<'a>(command: &'a CommandInteraction, ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
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
    let client = data_lock.get::<SubscriberClient>().ok_or(anyhow!("Subscriber client not found"))?.clone();

    let bot = match get_namespace().as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    let tx = parent_tx.start_child("subscribe_call", "subscribe");
    client.register_subscription(context::current(), subreddit.clone(), command.channel_id.get(), bot).await?;
    tx.finish();

    capture_event(ctx.data.clone(), "subscribe_subreddit", Some(parent_tx), Some(HashMap::from([("subreddit", subreddit.clone())])), &command.user.id.get().to_string()).await;

    return Ok(InteractionResponse {
        embed: Some(CreateEmbed::default()
            .title("Subscribed")
            .description(format!("This channel has been subscribed to r/{}", subreddit))
            .color(0x00ff00).to_owned()
        ),
        ..Default::default()
    });
}

async fn unsubscribe<'a>(command: &'a CommandInteraction, ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse, anyhow::Error> {
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
    let client = data_lock.get::<SubscriberClient>().ok_or(anyhow!("Subscriber client not found"))?.clone();

    let bot = match get_namespace().as_str() {
        "r-slash" => Bot::RS,
        "booty-bot" => Bot::BB,
        _ => Bot::RS,
    };

    debug!("Deadline: {:?}", context::current().deadline);
    debug!("Now: {:?}", SystemTime::now());
    let subreddits = match client.list_subscriptions(context::current(), command.channel_id.get(), bot).await? {
        Ok(x) => x,
        Err(_) => {
            return Err(anyhow!("Error getting subscriptions"));
        }
    };

    if subreddits.len() == 0 {
        return Ok(InteractionResponse {
            embed: Some(CreateEmbed::default()
                .title("No Subscriptions")
                .description("This channel has no subscriptions.")
                .color(0xff0000).to_owned()
            ),
            ephemeral: true,    
            ..Default::default()
        });
    }

    let menu = CreateSelectMenu::new(json!({"command": "unsubscribe"}).to_string(), CreateSelectMenuKind::String {
        options: subreddits.into_iter().map(|x| CreateSelectMenuOption::new("r/".to_owned() + &x.subreddit, x.subreddit)).collect(),
    })
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


#[instrument(skip(command, ctx, tx))]
async fn get_command_response<'a>(command: &'a CommandInteraction, ctx: &'a Context, tx: &'a sentry::Transaction) -> Result<InteractionResponse, anyhow::Error> {
    match command.data.name.as_str() {
        "ping" => {
            Ok(InteractionResponse {
                content: None,
                embed: Some(CreateEmbed::default()
                    .title("Pong!")
                    .color(Colour::from_rgb(0, 255, 0)).to_owned()
                ),
                components: None,
                file: None,
                ephemeral: false,
                fallback: ResponseFallbackMethod::Error
            })
        },

        "support" => {
            Ok(InteractionResponse {
                content: None,
                embed: Some(CreateEmbed::default()
                    .title("Get Support")
                    .description("[Discord Server](https://discord.gg/jYtCFQG)
                    Email: rslashdiscord@gmail.com")
                    .color(Colour::from_rgb(0, 255, 0))
                    .url("https://discord.gg/jYtCFQG").to_owned()
                ),
                components: None,
                file: None,
                ephemeral: false,
                fallback: ResponseFallbackMethod::Error
            })
        },

        "get" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_get_subreddit_cmd"));
            let resp = get_subreddit_cmd(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "membership" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_get_user_tiers"));
            let resp = cmd_get_user_tiers(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "custom" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_get_custom_subreddit"));
            let resp = get_custom_subreddit(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "info" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_info"));
            let resp = info(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "subscribe" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_subscribe"));
            let resp = subscribe(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "subscribe_custom" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_subscribe_custom"));
            let resp = subscribe_custom(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "unsubscribe" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_unsubscribe"));
            let resp = unsubscribe(&command, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        _ => {
            Err(anyhow!("Unknown command"))?
        }
    }
}


/// Discord event handler
struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client receives new data about a guild
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        debug!("Guild create event fired");
        let data_read = ctx.data.read().await;
        let conf = data_read.get::<ConfigStruct>().unwrap();
        let mut con = conf.redis.clone();
        let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace()), ctx.shard_id.0, ctx.cache.guild_count()).await.unwrap();

        if let Some(x) = is_new { // First time client has seen the guild
            if x {
                capture_event(ctx.data.clone(), "guild_join", None, None, &format!("guild_{}", guild.id.get().to_string())).await;
            }
        }
    }

    async fn guild_delete(&self, ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        {
            let data_read = ctx.data.read().await;    
            let conf = data_read.get::<ConfigStruct>().unwrap();
            let mut con = conf.redis.clone();
            let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace()), ctx.shard_id.0, ctx.cache.guild_count()).await.unwrap();
        }
    

        capture_event(ctx.data.clone(), "guild_leave", None, None, &format!("guild_{}", incomplete.id.get().to_string())).await;
    }

    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        capture_event(ctx.data, "on_ready", None, None, &format!("shard_{}", ready.shard.unwrap().id.0.to_string())).await;

        info!("Shard {} connected as {}, on {} servers!", ready.shard.unwrap().id.0, ready.user.name, ready.guilds.len());

        if !Path::new("/etc/probes").is_dir() {
            match fs::create_dir("/etc/probes") {
                Ok(_) => {},
                Err(e) => {
                    if !format!("{}", e).contains("File exists") {
                        error!("Error creating /etc/probes: {:?}", e);
                    }
                }
            }
        }
        if !Path::new("/etc/probes/live").exists() {
            let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
            file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
        }
    }

    /// Fires when the shard's status is updated
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        debug!("Shard stage changed to {:?}", event);
        let alive = match event.new {
            serenity::gateway::ConnectionStage::Connected => true,
            _ => false,
        };

        if alive {
            if !Path::new("/etc/probes/live").exists() {
                fs::create_dir_all("/etc/probes").expect("Couldn't create /etc/probes directory");
                let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
                file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
            }
        } else {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }

    /// Fires when a slash command or other interaction is received
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        let tx_ctx = sentry::TransactionContext::new("interaction_create", "interaction");
        let tx = sentry::start_transaction(tx_ctx);
        sentry::configure_scope(|scope| scope.set_span(Some(tx.clone().into())));

        if let Interaction::Command(command) = interaction.clone() {
            let slash_command_tx = tx.start_child("interaction.slash_command", "handle slash command");
            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : /{} {:?}", guild_id.name(&ctx.cache).unwrap_or("Name Unavailable".into()), guild_id.get(), command.user.name, command.user.id.get(), command.data.name, command.data.options);
                    info!("Sent at {:?}", command.id.created_at());
                },
                None => {
                    info!("{:?} ({:?}) : /{} {:?}", command.user.name, command.user.id.get(), command.data.name, command.data.options);
                    info!("Sent at {:?}", command.id.created_at());
                }
            }
            let command_response = get_command_response(&command, &ctx, &tx).await;
            let command_response = match command_response {
                Ok(embed) => embed,
                Err(why) => {
                    let why = why.to_string();
                    let code = rand::thread_rng().gen_range(0..10000);

                    info!("Error code {} getting command response: {:?}", code, why);

                    capture_event(ctx.data.clone(), "command_error", None, Some(HashMap::from([("shard_id", ctx.shard_id.to_string())])), &format!("user_{}", command.user.id)).await;
                    sentry::capture_message(&format!("Error getting command response: {:?}", why), sentry::Level::Error);

                    let map = HashMap::from([("content", format!("Error code {} getting command response: {:?}", code, why))]);
                    let client = reqwest::Client::new();
                    let _ = client.post("https://discord.com/api/webhooks/1065290872729649293/RmbUroqyxn6RXQythEdDtjIq4ztiYZ4dt1ZPSTxxwYK42GL0TB46E1rkRdG5xeVg7YfF")
                        .json(&map)
                        .send()
                        .await;

                    error_response(code.to_string())
                }
            };

            debug!("Sending response: {:?}", command_response);

            // Try to send response
            if let Err(why) = {
                let api_span = slash_command_tx.start_child("discord.api", "create slash command response");

                let mut resp = CreateInteractionResponseMessage::new();
                if let Some(embed) = command_response.embed.clone() {
                    resp = resp.embed(embed);
                };

                if let Some(components) = command_response.components.clone() {
                    resp = resp.components(components);
                };

                if let Some(content) = command_response.content.clone() {
                    resp = resp.content(content);
                };

                resp = resp.ephemeral(command_response.ephemeral);
                let to_return = command
                .create_response(&ctx.http, CreateInteractionResponse::Message(resp)).await;
                
                api_span.finish();
                to_return
            }
            {
                match why {
                    serenity::Error::Http(e) => {
                        if format!("{}", e) == "Interaction has already been acknowledged." {
                            debug!("Interaction already acknowledged, fallback is: {:?}", command_response.fallback);

                            // Interaction has already been responded to, we either need to edit the response, send a followup, error, or do nothing
                            // depending on the fallback method specified
                            match command_response.fallback {
                                ResponseFallbackMethod::Edit => {
                                    let api_span = slash_command_tx.start_child("discord.api", "edit slash command response");
                                    let mut resp = EditInteractionResponse::new();
                                    if let Some(embed) = command_response.embed.clone() {
                                        resp = resp.embed(embed);
                                    };
                    
                                    if let Some(components) = command_response.components.clone() {
                                        resp = resp.components(components);
                                    };
                    
                                    if let Some(content) = command_response.content.clone() {
                                        resp = resp.content(content);
                                    };
                    
                                    if let Err(why) = command.edit_response(&ctx.http, resp).await {
                                        warn!("Cannot edit slash command response: {}", why);
                                    };
                                    api_span.finish();
                                },

                                ResponseFallbackMethod::Followup => {
                                    let followup_span = slash_command_tx.start_child("discord.api", "send followup");
                                    let mut resp = CreateMessage::new();
                                    if let Some(embed) = command_response.embed.clone() {
                                        resp = resp.embed(embed);
                                    };
                    
                                    if let Some(components) = command_response.components.clone() {
                                        resp = resp.components(components);
                                    };
                    
                                    if let Some(content) = command_response.content.clone() {
                                        resp = resp.content(content);
                                    };
                    
                                    if let Err(why) = command.channel_id.send_message(&ctx.http, resp).await {
                                        warn!("Cannot send followup to slash command: {}", why);
                                    }
                                    followup_span.finish();
                                },

                                ResponseFallbackMethod::Error => {
                                    error!("Cannot respond to slash command: {}", e);
                                },

                                ResponseFallbackMethod::None => {}
                            };
                        }
                    }

                    _ => {
                        warn!("Cannot respond to slash command: {}", why);
                    }
                };
                slash_command_tx.finish();
            }
        }

        if let Interaction::Component(command) = interaction.clone() {
            let component_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.component", "handle component interaction"));

            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : Button {} {:?}", guild_id.name(&ctx.cache).unwrap_or("Name Unavailable".into()), guild_id.get(), command.user.name, command.user.id.get(), command.data.custom_id, command.data.kind);
                    info!("Sent at {:?}", command.id.created_at());
                },
                None => {
                    info!("{:?} ({:?}) : Button {} {:?}", command.user.name, command.user.id.get(), command.data.custom_id, command.data.kind);
                    info!("Sent at {:?}", command.id.created_at());
                }
            }

            // If custom_id uses invalid data structure (old version of bot), ignore interaction
            let custom_id: HashMap<String, serde_json::Value> = if let Ok(custom_id) = serde_json::from_str(&command.data.custom_id) {
                custom_id
            } else {
                return;
            };

            let button_command = match custom_id.get("command") {
                Some(command) => command.to_string().replace('"', ""),
                None => {
                    "again".to_string()
                }
            };

            let component_response: Option<Result<InteractionResponse, Error>> = match button_command.as_str() {
                "again" => {
                    let subreddit = custom_id["subreddit"].to_string().replace('"', "");
                    debug!("Search, {:?}", custom_id["search"]);
                    let search_enabled = match custom_id["search"] {
                        serde_json::Value::String(_) => true,
                        _ => false,
                    };
        
                    capture_event(ctx.data.clone(), "subreddit_cmd", Some(&component_tx), Some(HashMap::from([("subreddit", subreddit.clone().to_lowercase()), ("button", "true".to_string()), ("search_enabled", search_enabled.to_string())])), &format!("user_{}", command.user.id.get().to_string())).await;
                    
                    let data_read = ctx.data.read().await;
                    let conf = data_read.get::<ConfigStruct>().unwrap();
                    let mut con = conf.redis.clone();
        
                    let component_response = match search_enabled {
                        true => {
                            let search = custom_id["search"].to_string().replace('"', "");
                            match timeout(Duration::from_secs(30), get_subreddit_search(subreddit, search, &mut con, command.channel_id, Some(&component_tx))).await {
                                Ok(x) => x,
                                Err(x) => Err(anyhow!("Timeout getting search results: {:?}", x))
                            }
                        },
                        false => {
                            match timeout(Duration::from_secs(30), get_subreddit(subreddit, &mut con, command.channel_id, Some(&component_tx))).await {
                                Ok(x) => x,
                                Err(x) => Err(anyhow!("Timeout getting subreddit: {:?}", x))
                            }
                        }
                    };

                    Some(component_response)
                },

                "auto-post" => {
                    if let Some(member) = &command.member {
                        if !member.permissions(&ctx).unwrap().manage_messages() {
                            command.create_response(&ctx.http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content("You must have the 'Manage Messages' permission to setup auto-post.").ephemeral(true))).await.unwrap_or_else(|e| warn!("Failed to create response to autopost, {:?}", e));
                            return;
                        }
                    }

                    let is_premium = {
                        let data_read = ctx.data.read().await;
                        let mongodb_manager = data_read.get::<ResourceManager<mongodb::Client>>().unwrap().clone();
                        let mongodb_client_mutex = mongodb_manager.get_available_resource().await;
                        let mut mongodb_client = mongodb_client_mutex.lock().await;
                        let membership = get_user_tiers(command.user.id.get().to_string(), &mut *mongodb_client, Some(&component_tx)).await;
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
                            .max_length(6)
                        ),

                        CreateActionRow::InputText(
                            CreateInputText::new(InputTextStyle::Short, "Limit", "limit")
                            .label("Times to post before stopping e.g. 10")
                            .placeholder("Can be \"infinite\" if you have premium")
                            .min_length(1)
                            .max_length(max_length)
                        ),
                    ];


                    let resp = CreateInteractionResponse::Modal(
                        CreateModal::new(serde_json::to_string(&json!({
                            "subreddit": custom_id["subreddit"],
                            "command": "autopost",
                            "search": custom_id["search"]
                        })).unwrap(), "Autopost Delay")
                        .components(components)
                    );
                    match command.create_response(&ctx.http, resp).await {
                        Ok(_) => {},
                        Err(x) => {
                            warn!("Error sending modal: {:?}", x);
                        }
                        
                    };
                    None
                },

                "cancel_autopost" => {
                    if let Some(member) = &command.member {
                        if !member.permissions(&ctx).unwrap().manage_messages() {
                            command.create_response(&ctx.http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content("You must have the 'Manage Messages' permission to setup auto-post.").ephemeral(true))).await.unwrap_or_else(|e| {
                                warn!("Failed to create ephemeral message warning about missing 'Manage Messages' permission, {:?}", e);
                            });
                            return;
                        }
                    }
                    
                    command.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await.unwrap_or_else(|e| {
                        warn!("Failed to acknowledge cancel autopost, {:?}", e);
                    });
   


                    let lock = ctx.data.read().await;
                    let config = lock.get::<ConfigStruct>().unwrap();
                    let chan = config.auto_post_chan.clone();
                    match chan.send(AutoPostCommand::Stop(command.channel_id)).await {
                        Ok(_) => {},
                        Err(e) => {
                            error!("Failed to send stop autopost command: {:?}", e);
                            let mut config = lock.get::<ConfigStruct>().unwrap().clone();
                            drop(lock);
                            let auto_post_chan = tokio::sync::mpsc::channel(100);
                            config.auto_post_chan = auto_post_chan.0.clone();
                            let bot_name = ctx.cache.current_user().id.get().to_string();
                            tokio::spawn(poster::start_loop(auto_post_chan.1, ctx.data.clone(), ctx.http.clone(), ctx.shard_id.0.into(), bot_name));
                            let mut lock = ctx.data.write().await;
                            lock.insert::<ConfigStruct>(config);
                            info!("Restarted autopost loop");
                        }
                    };
                    capture_event(ctx.data.clone(), "autopost_cancel", Some(&component_tx), None, &format!("channel_{}", &command.channel.clone().unwrap().id.get().to_string())).await;
                    None
                },
                "unsubscribe" => {
                    Some(component_handlers::unsubscribe(&ctx, &command).await)
                },
                
                _ => {
                    warn!("Unknown button command: {}", button_command);
                    return;
                }
            };

            let component_response = match component_response {
                Some(component_response) => component_response,
                None => {
                    return;
                }
            };

            let component_response = match component_response {
                Ok(component_response) => component_response,
                Err(error) => {
                    let why = format!("{:?}", error);
                    let code = rand::thread_rng().gen_range(0..10000);
                    error!("Error code {} getting command response: {:?}", code, why);

                    capture_event(ctx.data.clone(), "command_error", Some(&component_tx), Some(HashMap::from([("shard_id", ctx.shard_id.to_string())])), &format!("user_{}", command.user.id)).await;
                    sentry::integrations::anyhow::capture_anyhow(&error);
                    let map = HashMap::from([("content", format!("Error code {} getting command response: {:?}", code, why))]);


                    let client = reqwest::Client::new();
                    let _ = client.post("https://discord.com/api/webhooks/1065290872729649293/RmbUroqyxn6RXQythEdDtjIq4ztiYZ4dt1ZPSTxxwYK42GL0TB46E1rkRdG5xeVg7YfF")
                        .json(&map)
                        .send()
                        .await;
                    error_response(code.to_string())
                }
            };

            if let Err(why) = {
                let api_span = component_tx.start_child("discord.api", "send button response");

                let mut resp = CreateInteractionResponseMessage::new();
                if let Some(embed) = component_response.embed.clone() {
                    resp = resp.embed(embed);
                };

                if let Some(components) = component_response.components.clone() {
                    resp = resp.components(components);
                };

                if let Some(content) = component_response.content.clone() {
                    resp = resp.content(content);
                };

                let to_return = command.create_response(&ctx.http, CreateInteractionResponse::Message(resp)).await;
                api_span.finish();
                to_return
            }
            {
                match why {
                    serenity::Error::Http(e) => {
                        if format!("{}", e) == "Interaction has already been acknowledged." {
                            debug!("Interaction already acknowledged, fallback is: {:?}", component_response.fallback);

                            // Interaction has already been responded to, we either need to edit the response, send a followup, error, or do nothing
                            // depending on the fallback method specified
                            match component_response.fallback {
                                ResponseFallbackMethod::Edit => {
                                    let api_span = component_tx.start_child("discord.api", "edit slash command response");
                                    let mut resp = EditInteractionResponse::new();
                                    if let Some(embed) = component_response.embed {
                                        resp = resp.embed(embed);
                                    };

                                    if let Some(components) = component_response.components {
                                        resp = resp.components(components);
                                    };

                                    if let Some(content) = component_response.content {
                                        resp = resp.content(content);
                                    };
                                    if let Err(why) = command.edit_response(&ctx.http, resp).await {
                                        warn!("Cannot edit slash command response: {}", why);
                                    };
                                    api_span.finish();
                                },

                                ResponseFallbackMethod::Followup => {
                                    let followup_span = component_tx.start_child("discord.api", "send followup");
                                    let mut resp = CreateMessage::new();
                                    if let Some(embed) = component_response.embed {
                                        resp = resp.embed(embed);
                                    };

                                    if let Some(components) = component_response.components {
                                        resp = resp.components(components);
                                    };

                                    if let Some(content) = component_response.content {
                                        resp = resp.content(content);
                                    };
                                    if let Err(why) = command.channel_id.send_message(&ctx.http, resp).await {
                                        warn!("Cannot send followup to slash command: {}", why);
                                    }
                                    followup_span.finish();
                                },

                                ResponseFallbackMethod::Error => {
                                    error!("Cannot respond to slash command: {}", e);
                                },

                                ResponseFallbackMethod::None => {}
                            };
                        }
                    }

                    _ => {
                        warn!("Cannot respond to slash command: {}", why);
                    }
                };
            }
            component_tx.finish();
        };
        
        if let Interaction::Modal(modal) = interaction.clone() {
            let modal_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.modal", "handle modal interaction"));

            match modal.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : Modal {} {:?}", guild_id.name(&ctx.cache).unwrap_or("Name Unavailable".into()), guild_id.get(), modal.user.name, modal.user.id.get(), modal.data.custom_id, modal.data.components);
                    info!("Sent at {:?}", modal.id.created_at());
                },
                None => {
                    info!("{:?} ({:?}) : Modal {} {:?}", modal.user.name, modal.user.id.get(), modal.data.custom_id, modal.data.components);
                    info!("Sent at {:?}", modal.id.created_at());
                }
            }

            // If custom_id uses invalid data structure (old version of bot), ignore interaction
            let custom_id: HashMap<String, serde_json::Value> = if let Ok(custom_id) = serde_json::from_str(&modal.data.custom_id) {
                custom_id
            } else {
                return;
            };

            let modal_command = match custom_id.get("command") {
                Some(command) => command.as_str().unwrap(),
                None => {
                    warn!("Unknown modal command: {:?}", custom_id);
                    modal_tx.finish();
                    return;
                }
            };

            match modal_command {
                "autopost" => {
                    capture_event(ctx.data.clone(), "autopost_start", Some(&modal_tx), None, &format!("channel_{}", modal.channel.clone().unwrap().id.get().to_string())).await;

                    let lock = ctx.data.read().await;
                    let config = lock.get::<ConfigStruct>().unwrap();
                    let chan = config.auto_post_chan.clone();

                    let search = match &custom_id["search"] {
                        serde_json::value::Value::Null => {
                            None
                        },
                        serde_json::value::Value::String(x) => {
                            Some(x.clone())
                        },
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
                                            _ => {
                                                "5s".to_string()
                                            }
                                        
                                        };
                                    } else if input.custom_id == "limit" {
                                        limit = match input.value.clone() {
                                            Some(x) => Some(x),
                                            _ => {
                                                Some("10".to_string())
                                            }
                                        };
                                    }
                                },
                                _ => {}
                            }
                        }
                    }

                    let is_premium = get_user_tiers(modal.user.id.get().to_string(), &mut *lock.get::<ResourceManager<mongodb::Client>>().unwrap().get_available_resource().await.lock().await, None).await.bronze.active;

                    let limit = match limit {
                        Some(x) => {
                            match x.parse::<u32>() {
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
                                            .embed(CreateEmbed::new()
                                                .title("Invalid Limit")
                                                .description(error_message)
                                                .color(0xff0000)
                                            )
                                            .ephemeral(true);

                                        match modal.create_response(&ctx.http, CreateInteractionResponse::Message(error_response)).await {
                                            Ok(_) => {},
                                            Err(e) => {
                                                error!("Failed to send error response: {:?}", e);
                                            }
                                        };
                                        return;
                                    }
                                }
                            }
                        },
                        None => None
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

                        match modal.create_response(&ctx.http, CreateInteractionResponse::Message(error_response)).await {
                            Ok(_) => {},
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
                        return;
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
                        return;
                    };

                    let interval = match interval {
                        Ok(x) => {
                            Duration::from_secs(x * (multiplier as u64))
                        },
                        Err(_) => {
                            invalid_interval().await;
                            return;
                        }
                    };

                    let post_request = rslash_types::PostRequest {
                        subreddit: custom_id["subreddit"].to_string().replace('"', ""),
                        interval,
                        search,
                        last_post: Instant::now() - interval,
                        current: 0,
                        limit,
                        channel: modal.channel_id,
                        author: modal.user.id,
                    };

                    match chan.send(AutoPostCommand::Start(post_request)).await {
                        Ok(_) => {},
                        Err(e) => {
                            error!("Failed to send start autopost command: {:?}", e);
                            let mut config = lock.get::<ConfigStruct>().unwrap().clone();
                            drop(lock);
                            let auto_post_chan = tokio::sync::mpsc::channel(100);
                            config.auto_post_chan = auto_post_chan.0.clone();
                            let bot_name = ctx.cache.current_user().id.get().to_string();
                            tokio::spawn(poster::start_loop(auto_post_chan.1, ctx.data.clone(), ctx.http.clone(), ctx.shard_id.0.into(), bot_name));
                            let mut lock = ctx.data.write().await;
                            lock.insert::<ConfigStruct>(config);
                            info!("Restarted autopost loop");

                            let error_response = CreateInteractionResponseMessage::new()
                                .embed(CreateEmbed::new()
                                    .title("Error Starting Autopost")
                                    .description("An internal error occurred while starting the autopost loop, please try again.")
                                    .color(0xff0000)
                                )
                                .ephemeral(true);

                            match modal.create_response(&ctx.http, CreateInteractionResponse::Message(error_response)).await {
                                Ok(_) => {},
                                Err(e) => {
                                    error!("Failed to send error response: {:?}", e);
                                }
                            };
                        }
                    };

                    modal.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await.unwrap_or_else(|e| {
                        warn!("Failed to acknowledge autopost modal: {:?}", e);
                        return;
                    });
                },

                _ => {
                    warn!("Unknown modal command: {}", modal_command);
                }
            }

            modal_tx.finish();
        }

        if let Interaction::Autocomplete(autocomplete) = interaction {
            let subreddit = match autocomplete.data.options.iter().find(|option| {
                option.name == "subreddit"
            }) {
                Some(option) => option.value.as_str().unwrap().to_string(),
                None => {
                    return;
                }
            };

            if subreddit.len() < 2 {
                return;
            }

            debug!("Autocomplete for {:?}", subreddit);

            let data_read = ctx.data.read().await;
            let conf = data_read.get::<ConfigStruct>().unwrap();
            let mut con = conf.redis.clone();

            let mut search = subreddit.replace('"', "\\");
            search = search.replace(":", "\\:");        

            let selector = if ctx.cache.current_user().id.get() == 278550142356029441 {
                "nsfw"
            } else {
                "sfw"
            };

            debug!("Getting autocomplete results for {:?}", search);
            let results: Vec<redis::Value> = match redis::cmd("FT.SEARCH")
                .arg(format!("{}_subs", selector))
                .arg(format!("*{}*", redis_sanitise(&search)))
                .arg("LIMIT")
                .arg(0)
                .arg(10)
                .query_async(&mut con).await {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Error getting autocomplete results: {:?}", e);
                        return;
                    }
            };

            let mut option_names = Vec::new();
            for result in results {
                if let redis::Value::Int(_) = result {
                    continue
                }

                if let Ok(x) = String::from_redis_value(&result) {
                    if x.starts_with("custom_sub") {
                        continue;
                    }
                }

                let result: Vec<redis::Value> = match result.into_sequence() {
                    Ok(x) => x,
                    Err(e) => { 
                        error!("Error getting autocomplete result: {:?}", e);
                        continue;
                    }
                };
                let name = match String::from_redis_value(&result[1]) {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Error getting autocomplete result: {:?}", e);
                        continue;
                    }
                };
                option_names.push(name);
            }

            debug!("Autocomplete results: {:?}", option_names);

            let resp = CreateAutocompleteResponse::new()
                .set_choices(option_names.into_iter().map(|x| AutocompleteChoice::new(x.clone(), x)).collect());

            match autocomplete.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp)).await {
                Ok(_) => {},
                Err(e) => {
                    error!("Failed to send autocomplete response: {:?}", e);
                }
            };
        }
        tx.finish();
    }
}


async fn monitor_total_shards(shard_manager: Arc<serenity::gateway::ShardManager>, total_shards: u32) {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_multiplexed_async_connection().await.expect("Can't connect to redis");

    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u32 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u32");

    loop {
        let _ = sleep(Duration::from_secs(60)).await;

        let db_total_shards: redis::RedisResult<u32> = con.get(format!("total_shards_{}", get_namespace())).await;
        let db_total_shards: u32 = db_total_shards.expect("Failed to get or convert total_shards from Redis");

        if !shard_manager.has(ShardId(shard_id)).await {
            debug!("Shard {} not found, marking self for termination.", shard_id);
            debug!("Instantiated shards: {:?}", shard_manager.shards_instantiated().await);
            let _ = fs::remove_file("/etc/probes/live");
        } else {
            if !Path::new("/etc/probes/live").exists() {
                debug!("Resurrected!");
                if !Path::new("/etc/probes").is_dir() {
                    fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
                }
                let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
                file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
            }
        }

        if db_total_shards != total_shards {
            debug!("Total shards changed from {} to {}, restarting.", total_shards, db_total_shards);
            shard_manager.set_shards(shard_id, 1, db_total_shards).await;
            shard_manager.initialize().expect("Failed to initialize shard");
        }
    }
}

fn get_namespace() -> String {
    let namespace= fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
    return namespace;
}

#[tokio::main]
async fn main() {    
    trace!("TRACE");
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u32 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u32");

    let redis_client = redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap();
    let mut con = redis_client.get_multiplexed_async_connection().await.expect("Can't connect to redis");

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Shard {}", shard_id));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("settings");

    let filter = doc! {"id": "subreddit_list".to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    let doc = cursor.try_next().await.unwrap().unwrap();

    let nsfw_subreddits: Vec<String> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap().to_string()).collect();

    let total_shards: redis::RedisResult<u32> = con.get(format!("total_shards_{}", get_namespace())).await;
    let total_shards: u32 = total_shards.expect("Failed to get or convert total_shards");

    debug!("Booting with {:?} total shards", total_shards);

    let mut client = serenity::Client::builder(token,  GatewayIntents::non_privileged())
        .event_handler(Handler)
        .application_id(application_id.into())
        .await
        .expect("Error creating client");



    let auto_post_chan = tokio::sync::mpsc::channel(100);
    {
        let mut data: tokio::sync::RwLockWriteGuard<'_, TypeMap> = client.data.write().await;

        let mongodb_manager = ResourceManager::<mongodb::Client>::new(|| Arc::new(Mutex::new(Box::pin(async {
            let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
            let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");

            let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
            client_options.app_name = Some(format!("Shard {}", shard_id));
            mongodb::Client::with_options(client_options).unwrap()
        })))).await;

        let posthog_manager = ResourceManager::<posthog::Client>::new(|| Arc::new(Mutex::new(Box::pin(async {
            let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
            posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string())
        })))).await;

        let reconnect_opts = ReconnectOptions::new()
        .with_exit_if_first_connect_fails(false)
        .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
        let tcp_stream = StubbornTcpStream::connect_with_options("post-subscriber.discord-bot-shared.svc.cluster.local:50051", reconnect_opts).await.expect("Failed to connect to post subscriber");
        let transport = Transport::from((tcp_stream, Bincode::default()));
        
        let subscriber = post_subscriber::SubscriberClient::new(tarpc::client::Config::default(), transport).spawn();

        data.insert::<ResourceManager<mongodb::Client>>(mongodb_manager);
        data.insert::<ResourceManager<posthog::Client>>(posthog_manager);
        data.insert::<ConfigStruct>(ConfigStruct {
            shard_id: shard_id,
            nsfw_subreddits: nsfw_subreddits,
            auto_post_chan: auto_post_chan.0.clone(),
            redis: con,
        });
        data.insert::<post_subscriber::SubscriberClient>(subscriber);
    }

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        debug!("Spawning shard monitor thread");
        monitor_total_shards(shard_manager, total_shards).await;
    });

    let bot_name = application_id.to_string();
    tokio::spawn(poster::start_loop(auto_post_chan.1, client.data.clone(), client.http.clone(), shard_id.into(), bot_name));

    let thread = tokio::spawn(async move {
        tracing_subscriber::Registry::default()
        .with(sentry::integrations::tracing::layer().span_filter(
            |md| {
                if md.name().contains("recv") || md.name().contains("recv_event") || md.name().contains("dispatch") || md.name().contains("handle_event") || md.name().contains("check_heartbeat") || md.name().contains("headers") {
                    return false
                } else {
                    return true;
                }
    
            }
        ).event_filter(|md| {
            let level_filter = match md.level() {
                &tracing::Level::ERROR => sentry::integrations::tracing::EventFilter::Event,
                &tracing::Level::WARN => sentry::integrations::tracing::EventFilter::Event,
                &tracing::Level::TRACE => sentry::integrations::tracing::EventFilter::Ignore,
                _ => sentry::integrations::tracing::EventFilter::Breadcrumb,
            };

            if (!md.target().contains("discord_shard")) || md.name().contains("serenity") || md.target().contains("serenity") {
                return sentry::integrations::tracing::EventFilter::Ignore;
            } else {
                return level_filter;
            }

        }))
        .with(tracing_subscriber::fmt::layer().compact().with_ansi(false).with_filter(tracing_subscriber::filter::LevelFilter::DEBUG).with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
            if !meta.target().contains("discord_shard") || meta.name().contains("serenity") {
                return false;
            };
            true
        })))
        .init();
    
        let bot_name: std::borrow::Cow<str>  = match &application_id {
            278550142356029441 => "booty-bot".into(),
            291255986742624256 => "testing".into(),
            _ => "r-slash".into()
        };

        let _guard = sentry::init(("https://e1d0fdcc5e224a40ae768e8d36dd7387@o4504774745718784.ingest.sentry.io/4504793832161280", sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 0.2,
            environment: Some(bot_name.clone()),
            server_name: Some(shard_id.to_string().into()),
            ..Default::default()
        }));
    
        debug!("Spawning client thread");
        client.start_shard(shard_id, total_shards).await.expect("Failed to start shard");
    });

    // If client thread exits, shard has crashed, so mark self as unhealthy.
    match thread.await {
        Ok(_) => {},
        Err(_) => {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }
}