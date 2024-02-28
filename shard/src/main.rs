use log::trace;
use serenity::all::{ActionRowComponent, InputTextStyle};
use serenity::gateway::ShardStageUpdateEvent;
use serenity::model::Colour;
use tracing::{debug, info, warn, error};
use rand::Rng;
use serde_json::json;
use tracing::instrument;
use tracing_subscriber::Layer;
use tracing_subscriber::prelude::__tracing_subscriber_SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use std::fs;
use std::io::Write;
use std::collections::HashMap;
use std::env;

use serenity::builder::{CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor, CreateEmbedFooter, CreateInputText, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage, CreateModal, EditInteractionResponse};
use serenity::model::id::{ChannelId, ShardId};
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

use redis;
use redis::{AsyncCommands, from_redis_value};
use mongodb::options::ClientOptions;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;

use serenity::model::guild::{Guild, UnavailableGuild};
use serenity::model::application::{Interaction, CommandInteraction, ButtonStyle};

use anyhow::{anyhow, Error};

use memberships::*;
use connection_pooler::ResourceManager;

mod poster;
mod types;

use crate::poster::AutoPostCommand;
use crate::types::ConfigStruct;


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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

    debug!("{:?}", client.capture(event, properties_map, distinct_id).await.unwrap().text().await.unwrap());
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


#[instrument(skip(con, parent_tx))]
async fn get_length_of_search_results(search_index: String, search: String, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<u16, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_length_of_search_results").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_length_of_search_results");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut new_search = String::new();
    for c in search.chars() {
        if c.is_whitespace() && c != ' ' && c != '_' {
            new_search.push('\\');
            new_search.push(c);
        }
        else {
            new_search.push(c);
        }
    }

    debug!("Getting length of search results for {} in {}", new_search, search_index);
    let results: Vec<u16> = redis::cmd("FT.SEARCH")
        .arg(search_index)
        .arg(new_search)
        .arg("LIMIT")
        .arg(0) // Return no results, just number of results
        .arg(0)
        .query_async(con).await?;

    span.finish();
    Ok(results[0])
}


// Returns the post ID at the given index in the search results
#[instrument(skip(con, parent_span))]
async fn get_post_at_search_index(search_index: String, search: &str, index: u16, con: &mut redis::aio::MultiplexedConnection, parent_span: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_span {
        Some(parent) => parent.start_child("db.query", "get_post_at_search_index").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_at_search_index");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut new_search = String::new();
    for c in search.chars() {
        if c.is_whitespace() && c != ' ' && c != '_' {
            new_search.push('\\');
            new_search.push(c);
        }
        else {
            new_search.push(c);
        }
    }

    let results: Vec<redis::Value> = redis::cmd("FT.SEARCH")
        .arg(search_index)
        .arg(new_search)
        .arg("LIMIT")
        .arg(index)
        .arg(1)
        .arg("SORTBY")
        .arg("score")
        .arg("NOCONTENT") // Only show POST IDs not post content
        .query_async(con).await?;

    let to_return = from_redis_value::<String>(&results[1])?;
    span.finish();
    Ok(to_return)
}


// Returns the post ID at the given index in the list
#[instrument(skip(con, parent_tx))]
async fn get_post_at_list_index(list: String, index: u16, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
    debug!("Getting post at index {} in list {}", index, list);

    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_post_at_list_index").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_at_list_index");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut results: Vec<String> = redis::cmd("LRANGE")
        .arg(list)
        .arg(index)
        .arg(index)
        .query_async(con).await?;

    span.finish();
    Ok(results.remove(0))
}


#[instrument(skip(con, parent_tx))]
async fn get_post_by_id<'a>(post_id: String, search: Option<String>, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_post_by_id").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_by_id");
            sentry::start_transaction(ctx).into()
        }
    };

    let post: HashMap<String, redis::Value> = con.hgetall(&post_id).await?;

    let subreddit = post_id.split(":").collect::<Vec<&str>>()[1].to_string();

    let author = from_redis_value::<String>(&post.get("author").unwrap().clone())?;
    let title = from_redis_value::<String>(&post.get("title").unwrap().clone())?;
    let url = from_redis_value::<String>(&post.get("url").unwrap().clone())?;
    let embed_url = from_redis_value::<String>(&post.get("embed_url").unwrap().clone())?;
    let timestamp = from_redis_value::<i64>(&post.get("timestamp").unwrap().clone())?;
    
    let to_return = InteractionResponse {
        embed: Some(CreateEmbed::default()
            .title(title)
            .description(format!("r/{}", subreddit))
            .author(CreateEmbedAuthor::new(format!("u/{}", author))
                .url(format!("https://reddit.com/u/{}", author))
            )
            .url(url)
            .color(0x00ff00)
            .image(embed_url)
            .timestamp(serenity::model::timestamp::Timestamp::from_unix_timestamp(timestamp)?)
            .to_owned()
        ),

        components: Some(vec![CreateActionRow::Buttons(vec![
            CreateButton::new(json!({
                "subreddit": subreddit,
                "search": search,
                "command": "again"
            }).to_string())
                .label("🔁")
                .style(ButtonStyle::Primary),

            CreateButton::new(json!({
                "subreddit": subreddit,
                "search": search,
                "command": "auto-post"
            }).to_string())
                .label("Auto-Post")
                .style(ButtonStyle::Primary),
        ])]),

        fallback: ResponseFallbackMethod::Edit,
        ..Default::default()
    };

    span.finish();
    return Ok(to_return);
}


#[instrument(skip(con, parent_tx))]
pub async fn get_subreddit_search<'a>(subreddit: String, search: String, con: &mut redis::aio::MultiplexedConnection, channel: ChannelId, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("subreddit.search", "get_subreddit_search").into(),
        None => {
            let ctx = sentry::TransactionContext::new("subreddit.search", "get_subreddit_search");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut index: u16 = con.incr(format!("subreddit:{}:search:{}:channels:{}:index", &subreddit, &search, channel), 1i16).await?;
    let _:() = con.expire(format!("subreddit:{}:search:{}:channels:{}:index", &subreddit, &search, channel), 60*60).await?;
    index -= 1;
    let length: u16 = get_length_of_search_results(format!("idx:{}", &subreddit), search.clone(), con, Some(&span)).await?;

    if length == 0 {
        return Ok(InteractionResponse {
            embed: Some(CreateEmbed::default()
                .title("No search results found")
                .color(0xff0000)
                .to_owned()
            ),
            ..Default::default()
        });
    }

    index = length - (index + 1);

    if index >= length {
        let _:() = con.set(format!("subreddit:{}:search:{}:channels:{}:index", &subreddit, &search, channel), 0i16).await?;
        index = 0;
    }

    let post_id = get_post_at_search_index(format!("idx:{}", &subreddit), &search, index, con, Some(&span)).await?;
    let to_return = get_post_by_id(post_id, Some(search), con, Some(&span)).await?;
    span.finish();
    return Ok(to_return);
}


#[instrument(skip(con, parent_tx))]
pub async fn get_subreddit<'a>(subreddit: String, con: &mut redis::aio::MultiplexedConnection, channel: ChannelId, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("subreddit.get", "get_subreddit").into(),
        None => {
            let ctx = sentry::TransactionContext::new("subreddit.get", "get_subreddit");
            sentry::start_transaction(ctx).into()
        }
    };

    let subreddit = subreddit.to_lowercase();

    let mut index: u16 = con.incr(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 1i16).await?;
    let _:() = con.expire(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 60*60).await?;
    index -= 1;
    let length: u16 = con.llen(format!("subreddit:{}:posts", &subreddit)).await?;

    if index >= length {
        let _:() = con.set(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 0i16).await?;
        index = 0;
    }

    let post_id = get_post_at_list_index(format!("subreddit:{}:posts", &subreddit), index, con, Some(&span)).await?;

    let to_return = get_post_by_id(post_id, None, con, Some(&span)).await?;
    span.finish();
    return Ok(to_return);
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
        ephemeral: false,
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

#[instrument(skip(con, parent_tx))]
async fn list_contains(element: &str, list: &str, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<bool, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "list_contains").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "list_contains");
            sentry::start_transaction(ctx).into()
        }
    };

    let position: Option<u16> = con.lpos(list, element, redis::LposOptions::default()).await?;

    let to_return = match position {
        Some(_) => Ok(true),
        None => Ok(false),
    };

    span.finish();
    to_return
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
                Get it here: https://ko-fi.com/rslash")
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
        .get(format!("https://www.reddit.com/r/{}.json", subreddit))
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
        debug!("Subreddit last cached more than an hour ago, updating...");
        command.defer(&ctx.http).await.unwrap_or_else(|e| {
            warn!("Failed to defer response: {}", e);
        });
        if !already_queued {
            debug!("Queueing subreddit for download");
            con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
        loop {
            sleep(Duration::from_millis(1000)).await;

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
            .description(format!("[Support Server](https://discord.gg/jYtCFQG)
        
            [Add me to a server](https://discord.com/api/oauth2/authorize?client_id={}&permissions=515463498752&scope=applications.commands%20bot)
            
            [Privacy Policy](https://pastebin.com/DtZvJJhG)
            [Terms & Conditions](https://pastebin.com/6c4z3uM5)", id))
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
        ..Default::default()
    })
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

        _ => {
            Err(anyhow!("Unknown command"))?
        }
    }
}


/// What to do if sending a response fails due to already being acknowledged
#[derive(Debug, Clone)]
enum ResponseFallbackMethod {
    /// Edit the original response
    Edit,
    /// Send a followup response
    Followup,
    /// Return an error
    Error,
    /// Do nothing
    None
}

#[derive(Debug, Clone)]
struct InteractionResponse {
    file: Option<serenity::builder::CreateAttachment>,
    embed: Option<serenity::builder::CreateEmbed>,
    content: Option<String>,
    ephemeral: bool,
    components: Option<Vec<CreateActionRow>>,
    fallback: ResponseFallbackMethod,
}

impl Default for InteractionResponse {
    fn default() -> Self {
        InteractionResponse {
            file: None,
            embed: None,
            content: None,
            ephemeral: false,
            components: None,
            fallback: ResponseFallbackMethod::Error
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
            fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
        }
        if !Path::new("/etc/probes/live").exists() {
            let mut file = File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
            file.write_all(b"alive").expect("Unable to write to /etc/probes/live");
        }
    }

    /// Fires when the shard's status is updated
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        debug!("Shard stage changed");
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
        
                    capture_event(ctx.data.clone(), "subreddit_cmd", Some(&component_tx), Some(HashMap::from([("subreddit", subreddit.clone()), ("button", "true".to_string()), ("search_enabled", search_enabled.to_string())])), &format!("user_{}", command.user.id.get().to_string())).await;
                    
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

                    let resp = CreateInteractionResponse::Modal(
                        CreateModal::new(serde_json::to_string(&json!({
                            "subreddit": custom_id["subreddit"],
                            "command": "autopost",
                            "search": custom_id["search"]
                        })).unwrap(), "Autopost Delay")
                        .components(vec![
                            CreateActionRow::InputText(
                                CreateInputText::new(InputTextStyle::Short, "Delay", "delay")
                                .label("Delay e.g. 5s, 3m, 5h, 1d")
                                .placeholder("5s")
                                .min_length(2)
                                .max_length(6)
                            ),

                            CreateActionRow::InputText(
                                CreateInputText::new(InputTextStyle::Short, "Limit", "limit")
                                .label("How many times to post in total")
                                .placeholder("10")
                                .min_length(1)
                                .max_length(2)
                            ),
                        ])
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
                    command.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await.unwrap();
                    if let Some(member) = &command.member {
                        if !member.permissions(&ctx).unwrap().manage_messages() {
                            command.create_response(&ctx.http, CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content("You must have the 'Manage Messages' permission to setup auto-post.").ephemeral(true))).await.unwrap();
                            return;
                        }
                    }


                    let lock = ctx.data.read().await;
                    let config = lock.get::<ConfigStruct>().unwrap();
                    let chan = config.auto_post_chan.clone();
                    chan.send(AutoPostCommand::Stop(command.channel_id)).await.unwrap();
                    capture_event(ctx.data.clone(), "autopost_cancel", Some(&component_tx), None, &format!("channel_{}", &command.channel.clone().unwrap().id.get().to_string())).await;
                    None
                }

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
        
        if let Interaction::Modal(modal) = interaction {
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
                    modal.create_response(&ctx.http, CreateInteractionResponse::Acknowledge).await.unwrap();
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
                    let mut limit = String::new();

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
                                            Some(x) => x,
                                            _ => {
                                                "10".to_string()
                                            }
                                        };
                                    }
                                },
                                _ => {}
                            }
                        }
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
                        return;
                    };

                    let interval = match interval {
                        Ok(x) => {
                            Duration::from_secs(x * (multiplier as u64))
                        },
                        Err(_) => {
                            debug!("Invalid interval: {:?}", interval);
                            return;
                        }
                    };

                    let limit = match limit.parse::<u32>() {
                        Ok(x) => x,
                        Err(_) => {
                            debug!("Invalid limit: {:?}", limit);
                            return;
                        }
                    };

                    let post_request = poster::PostRequest {
                        subreddit: custom_id["subreddit"].to_string().replace('"', ""),
                        interval,
                        search,
                        last_post: Instant::now() - interval,
                        current: 0,
                        limit,
                        channel: modal.channel_id,
                        author: modal.user.id,
                    };

                    chan.send(AutoPostCommand::Start(post_request)).await.unwrap();
                },

                _ => {
                    warn!("Unknown modal command: {}", modal_command);
                }
            }

            modal_tx.finish();
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

    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
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

            let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
            client_options.app_name = Some(format!("Shard {}", shard_id));
            mongodb::Client::with_options(client_options).unwrap()
        })))).await;

        let posthog_manager = ResourceManager::<posthog::Client>::new(|| Arc::new(Mutex::new(Box::pin(async {
            let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
            posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string())
        })))).await;

        data.insert::<ResourceManager<mongodb::Client>>(mongodb_manager);
        data.insert::<ResourceManager<posthog::Client>>(posthog_manager);
        data.insert::<ConfigStruct>(ConfigStruct {
            shard_id: shard_id,
            nsfw_subreddits: nsfw_subreddits,
            auto_post_chan: auto_post_chan.0.clone(),
            redis: con,
        });
    }

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        debug!("Spawning shard monitor thread");
        monitor_total_shards(shard_manager, total_shards).await;
    });

    tokio::spawn(poster::start_loop(auto_post_chan.1, client.data.clone(), client.http.clone()));

    let thread = tokio::spawn(async move {
        tracing_subscriber::Registry::default()
        .with(sentry::integrations::tracing::layer().span_filter(
            |md| {
                if md.name().contains("recv") || md.name().contains("recv_event") || md.name().contains("dispatch") || md.name().contains("handle_event") || md.name().contains("check_heartbeat") {
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