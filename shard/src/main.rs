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
use std::iter::FromIterator;
use std::env;
use chrono::{DateTime, Utc, TimeZone};

use serenity::builder::{CreateEmbed, CreateComponents};
use serenity::model::id::{ChannelId};
use serenity::model::gateway::GatewayIntents;
use serenity::{
    async_trait,
    model::gateway::Ready,
    prelude::*,
};
use serenity::model::application::interaction::application_command::ApplicationCommandInteraction;
use serenity::model::application::interaction::InteractionResponseType;
use serenity::model::application::interaction::Interaction;

use tokio::time::{sleep, Duration};
use std::time::{SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use std::fs::File;
use std::path::Path;
use futures_util::TryStreamExt;

use redis;
use redis::{AsyncCommands, from_redis_value};
use mongodb::options::ClientOptions;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;

use serenity::client::bridge::gateway::event::ShardStageUpdateEvent;
use serenity::model::application::component::ButtonStyle;
use serenity::model::guild::{Guild, UnavailableGuild};
use serenity::utils::Colour;

use anyhow::anyhow;

use memberships::*;
use rslash_types::*;


/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[instrument(skip(data, parent_tx))]
async fn capture_event(data: &mut HashMap<String, ConfigValue>, event: &str, parent_tx: Option<&sentry::TransactionOrSpan>, properties: Option<HashMap<&str, String>>, distinct_id: &str) {
    let span: sentry::TransactionOrSpan = match parent_tx {
        Some(parent) => parent.start_child("analytics", "capture_event").into(),
        None => {
            let ctx = sentry::TransactionContext::new("analytics", "capture_event");
            sentry::start_transaction(ctx).into()
        }
    };

    let posthog_client = match data.get_mut("posthog_client").unwrap() {
        ConfigValue::PosthogClient(client) => Ok(client),
        _ => Err("posthog_client is not a PosthogClient"),
    }.unwrap();

    let mut properties_map = serde_json::Map::new();
    if properties.is_some() {
        for (key, value) in properties.unwrap() {
            properties_map.insert(key.to_string(), serde_json::Value::String(value));
        }
    }

    debug!("{:?}", posthog_client.capture(event, properties_map, distinct_id).await.unwrap().text().await.unwrap());
    span.finish();
}


#[instrument(skip(command, data, ctx, tx))]
async fn get_subreddit_cmd<'a, 'b>(command: &'a ApplicationCommandInteraction, data: &'b mut tokio::sync::RwLockWriteGuard<'a, TypeMap>, ctx: &'a Context, tx: &'b sentry::TransactionOrSpan) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    let nsfw_subreddits = match data_mut.get_mut("nsfw_subreddits").unwrap() {
        ConfigValue::SubredditList(list) => Ok(list),
        _ => Err(anyhow!("nsfw_subreddits is not a list")),
    }?;

    let nsfw_subreddits = nsfw_subreddits.clone();

    let options = &command.data.options;
    debug!("Command Options: {:?}", options);

    let subreddit = options[0].value.clone();
    let subreddit = subreddit.unwrap();
    let subreddit = subreddit.as_str().unwrap().to_string();

    let search_enabled = options.len() > 1;
    capture_event(data_mut, "subreddit_cmd", Some(tx),
                    Some(HashMap::from([("subreddit", subreddit.clone()), ("button", "false".to_string()), ("search_enabled", search_enabled.to_string())])), &format!("user_{}", command.user.id.0.to_string())
                ).await;

    let mut con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(anyhow!("redis_connection is not a redis connection")),
    }?;

    if nsfw_subreddits.contains(&subreddit) {
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

    if options.len() > 1 {
        let search = options[1].value.as_ref().unwrap().as_str().unwrap().to_string();
        return get_subreddit_search(subreddit, search, &mut con, command.channel_id, Some(tx)).await
    }
    else {
        return get_subreddit(subreddit, &mut con, command.channel_id, Some(tx)).await
    }
}


#[instrument(skip(con, parent_tx))]
async fn get_length_of_search_results(search_index: String, search: String, con: &mut redis::aio::Connection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<u16, anyhow::Error> {
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
async fn get_post_at_search_index(search_index: String, search: &str, index: u16, con: &mut redis::aio::Connection, parent_span: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
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
async fn get_post_at_list_index(list: String, index: u16, con: &mut redis::aio::Connection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
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
async fn get_post_by_id<'a>(post_id: String, search: Option<String>, con: &mut redis::aio::Connection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_post_by_id").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_by_id");
            sentry::start_transaction(ctx).into()
        }
    };

    let post: HashMap<String, redis::Value> = con.hgetall(&post_id).await?;

    let subreddit = post_id.split(":").collect::<Vec<&str>>()[1].to_string();

    let custom_data = json!({
        "subreddit": subreddit,
        "search": search,
    }).to_string();

    let author = from_redis_value::<String>(&post.get("author").unwrap().clone())?;
    let title = from_redis_value::<String>(&post.get("title").unwrap().clone())?;
    let url = from_redis_value::<String>(&post.get("url").unwrap().clone())?;
    let embed_url = from_redis_value::<String>(&post.get("embed_url").unwrap().clone())?;
    let timestamp = from_redis_value::<i64>(&post.get("timestamp").unwrap().clone())?;
    
    let to_return = InteractionResponse {
        embed: Some(CreateEmbed::default()
            .title(title)
            .description(format!("r/{}", subreddit))
            .author(|a|
                a.name(format!("u/{}", author))
                .url(format!("https://reddit.com/u/{}", author))
            )
            .url(url)
            .color(0x00ff00)
            .image(embed_url)
            .timestamp(serenity::model::timestamp::Timestamp::from_unix_timestamp(timestamp)?)
            .to_owned()
        ),

        components: Some(CreateComponents::default()
            .create_action_row(|a|
                a.create_button(|b|
                    b.label("🔁")
                    .style(ButtonStyle::Primary)
                    .custom_id(custom_data)
                )
            ).to_owned()
        ),

        fallback: ResponseFallbackMethod::Edit,
        ..Default::default()
    };

    span.finish();
    return Ok(to_return);
}


#[instrument(skip(con, parent_tx))]
async fn get_subreddit_search<'a>(subreddit: String, search: String, con: &mut redis::aio::Connection, channel: ChannelId, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse<'a>, anyhow::Error> {
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
async fn get_subreddit<'a>(subreddit: String, con: &mut redis::aio::Connection, channel: ChannelId, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("subreddit.get", "get_subreddit").into(),
        None => {
            let ctx = sentry::TransactionContext::new("subreddit.get", "get_subreddit");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut index: u16 = con.incr(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 1i16).await?;
    let _:() = con.expire(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 60*60).await?;
    index -= 1;
    let length: u16 = con.llen(format!("subreddit:{}:posts", &subreddit)).await?;
    index = length - (index + 1);

    if index >= length {
        let _:() = con.set(format!("subreddit:{}:channels:{}:index", &subreddit, channel), 0i16).await?;
        index = 0;
    }

    let post_id = get_post_at_list_index(format!("subreddit:{}:posts", &subreddit), index, con, Some(&span)).await?;

    let to_return = get_post_by_id(post_id, None, con, Some(&span)).await?;
    span.finish();
    return Ok(to_return);
}


fn error_response(code: String) -> InteractionResponse<'static> {
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
    };
}


#[instrument(skip(command, data, _ctx, parent_tx))]
async fn cmd_get_user_tiers<'a>(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, _ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();
    let mongodb_client = match data_mut.get_mut("mongodb_connection").unwrap() {
        ConfigValue::MONGODB(db) => Ok(db),
        _ => Err(anyhow!("Failed to get mongodb connection")),
    }?;

    let tiers = get_user_tiers(command.user.id.0.to_string(), mongodb_client, Some(parent_tx)).await;
    debug!("Tiers: {:?}", tiers);

    let bronze = match tiers.bronze.active {
        true => "Active",
        false => "Inactive",
    }.to_string();


    capture_event(data_mut, "cmd_get_user_tiers", Some(parent_tx), Some(HashMap::from([("bronze_active", bronze.to_string())])), &format!("user_{}", command.user.id.0.to_string())).await;

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
async fn list_contains(element: &str, list: &str, con: &mut redis::aio::Connection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<bool, anyhow::Error> {
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
async fn get_custom_subreddit<'a, 'b>(command: &'a ApplicationCommandInteraction, ctx: &'a Context, parent_tx: &'b sentry::TransactionOrSpan) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let mut data = ctx.data.write().await;

    let membership = get_user_tiers(command.user.id.0.to_string(), &mut data, Some(parent_tx)).await;
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

    drop(data); // Release lock while performing web request

    let options = &command.data.options;
    let subreddit = options[0].value.clone();
    let subreddit = subreddit.unwrap();
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

    let mut data = ctx.data.write().await;
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    let con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(anyhow!("redis_connection is not a redis connection")),
    }?;

    let already_queued = list_contains(&subreddit, "custom_subreddits_queue", con, Some(parent_tx)).await?;

    let last_cached: i64 = con.get(&format!("{}", subreddit)).await.unwrap_or(0);

    drop(data);
    if last_cached == 0 {
        debug!("Subreddit last cached more than an hour ago, updating...");
        command.defer(&ctx.http).await.unwrap_or_else(|e| {
            warn!("Failed to defer response: {}", e);
        });
        if !already_queued {
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        
            let con = match data_mut.get_mut("redis_connection").unwrap() {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(anyhow!("redis_connection is not a redis connection")),
            }?;
        
            let _:() = con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
        loop {
            sleep(Duration::from_millis(1000)).await;
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();

            let con = match data_mut.get_mut("redis_connection").unwrap() {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(anyhow!("redis_connection is not a redis connection")),
            }?;

            let posts: Vec<String> = match redis::cmd("LRANGE").arg(format!("subreddit:{}:posts", subreddit.clone())).arg(0i64).arg(0i64).query_async(con).await {
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
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        
            let con = match data_mut.get_mut("redis_connection").unwrap() {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(anyhow!("redis_connection is not a redis connection")),
            }?;
        
            let _:() = con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
    }

    let mut data = ctx.data.write().await;

    return get_subreddit_cmd(command, &mut data, ctx, parent_tx).await;
}

#[instrument(skip(command, data, ctx, parent_tx))]
async fn info<'a>(command: &ApplicationCommandInteraction, data: &mut tokio::sync::RwLockWriteGuard<'_, TypeMap>, ctx: &Context, parent_tx: &sentry::TransactionOrSpan) -> Result<InteractionResponse<'a>, anyhow::Error> {
    let data_mut = data.get_mut::<ConfigStruct>().unwrap();

    capture_event(data_mut, "cmd_info", Some(parent_tx), None, &format!("user_{}", command.user.id.0.to_string())).await;

    let con = match data_mut.get_mut("redis_connection").unwrap() {
        ConfigValue::REDIS(db) => Ok(db),
        _ => Err(anyhow!("redis_connection is not a redis connection")),
    }?;

    let guild_counts: HashMap<String, redis::Value> = con.hgetall(format!("shard_guild_counts_{}", get_namespace().await)).await?;
    let mut guild_count = 0;
    for (_, count) in guild_counts {
        guild_count += from_redis_value::<u64>(&count)?;
    }

    let id = ctx.cache.current_user_id().0;

    return Ok(InteractionResponse {
        embed: Some(CreateEmbed::default()
            .title("Info")
            .description(format!("[Support Server](https://discord.gg/jYtCFQG)
        
            [Add me to a server](https://discord.com/api/oauth2/authorize?client_id={}&permissions=515463498752&scope=applications.commands%20bot)
            
            [Privacy Policy](https://pastebin.com/DtZvJJhG)
            [Terms & Conditions](https://pastebin.com/6c4z3uM5)", id))
            .color(0x00ff00).to_owned()
            .footer(|f| f.text(format!("v{} compiled at {}", env!("CARGO_PKG_VERSION"), compile_time::datetime_str!())))
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
async fn get_command_response<'a>(command: &'a ApplicationCommandInteraction, ctx: &'a Context, tx: &'a sentry::Transaction) -> Result<InteractionResponse<'a>, anyhow::Error> {
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
            let mut data = ctx.data.write().await;
            debug!("Got lock");
            let resp = get_subreddit_cmd(&command, &mut data, &ctx, &cmd_tx).await;
            cmd_tx.finish();
            resp
        },

        "membership" => {
            let cmd_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.slash_command.get_response", "cmd_get_user_tiers"));
            let mut data = ctx.data.write().await;
            debug!("Got lock");
            let resp = cmd_get_user_tiers(&command, &mut data, &ctx, &cmd_tx).await;
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
            let mut data = ctx.data.write().await;
            debug!("Got lock");
            let resp = info(&command, &mut data, &ctx, &cmd_tx).await;
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
struct InteractionResponse<'a> {
    file: Option<serenity::model::channel::AttachmentType<'a>>,
    embed: Option<serenity::builder::CreateEmbed>,
    content: Option<String>,
    ephemeral: bool,
    components: Option<serenity::builder::CreateComponents>,
    fallback: ResponseFallbackMethod
}

impl Default for InteractionResponse<'_> {
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
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: bool) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();

        let con = match data_mut.get_mut("redis_connection").unwrap() {
            ConfigValue::REDIS(db) => Ok(db),
            _ => Err(0),
        }.unwrap();

        let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace().await), ctx.shard_id, ctx.cache.guild_count()).await.unwrap();

        if is_new { // First time client has seen the guild
            capture_event(data_mut, "guild_join", None, None, &format!("guild_{}", guild.id.0.to_string())).await;
            //update_guild_commands(guild.id, &mut data, &ctx).await;
        }
    }

    async fn guild_delete(&self, ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();

        capture_event(data_mut, "guild_leave", None, None, &format!("guild_{}", incomplete.id.0.to_string())).await;

        let con = match data_mut.get_mut("redis_connection").unwrap() {
            ConfigValue::REDIS(db) => Ok(db),
            _ => Err(0),
        }.unwrap();

        let _:() = con.hset(format!("shard_guild_counts_{}", get_namespace().await), ctx.shard_id, ctx.cache.guild_count()).await.unwrap();
    }

    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        capture_event(data_mut, "on_ready", None, None, &format!("shard_{}", ready.shard.unwrap()[0].to_string())).await;

        info!("Shard {} connected as {}, on {} servers!", ready.shard.unwrap()[0], ready.user.name, ready.guilds.len());

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
        
        // Check if there is a deadlock
        {
            let try_write = ctx.data.try_write();
            if try_write.is_err() {
                debug!("Couldn't get data lock immediately");
            } else {
                debug!("Got data lock");
            }
        }

        if let Interaction::ApplicationCommand(command) = interaction.clone() {
            let slash_command_tx = tx.start_child("interaction.slash_command", "handle slash command");
            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : /{} {:?}", guild_id.name(&ctx.cache).unwrap_or("Name Unavailable".into()), guild_id.as_u64(), command.user.name, command.user.id.as_u64(), command.data.name, command.data.options);
                },
                None => {
                    info!("{:?} ({:?}) : /{} {:?}", command.user.name, command.user.id.as_u64(), command.data.name, command.data.options);
                }
            }
            let command_response = get_command_response(&command, &ctx, &tx).await;
            let command_response = match command_response {
                Ok(embed) => embed,
                Err(why) => {
                    let why = why.to_string();
                    let code = rand::thread_rng().gen_range(0..10000);

                    info!("Error code {} getting command response: {:?}", code, why);

                    let mut data = ctx.data.write().await;
                    let data_mut = data.get_mut::<ConfigStruct>().expect("Couldn't get config struct");

                    capture_event(data_mut, "command_error", None, Some(HashMap::from([("shard_id", ctx.shard_id.to_string())])), &format!("user_{}", command.user.id)).await;
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
                let to_return = command
                .create_interaction_response(&ctx.http, |response| {
                    let span = slash_command_tx.start_child("discord.prepare_response", "create slash command response");
                    let to_return = response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|mut message| {
                            // Clone component_response so we can use it in the closure without moving the original data
                            let command_response = command_response.clone();
                            if command_response.embed.is_some() {
                                message = message.set_embed(command_response.embed.unwrap());
                            };

                            if command_response.components.is_some() {
                                message = message.set_components(command_response.components.unwrap());
                            };

                            if command_response.content.is_some() {
                                message = message.content(command_response.content.unwrap());
                            };

                            message = message.ephemeral(command_response.ephemeral);

                            message
                        });

                    span.finish();
                    to_return
                })
                .await;
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
                                    if let Err(why) = command.edit_original_interaction_response(&ctx.http, |mut message| {
                                        // Clone component_response so we can use it in the closure without moving the original data
                                        let command_response = command_response.clone();
                                        if command_response.embed.is_some() {
                                            message = message.set_embed(command_response.embed.unwrap());
                                        };
            
                                        if command_response.components.is_some() {
                                            message = message.components(|msg| {
                                                // Copy properties across, need to do this because for some reason edit_original_interaction_response doesn't provide set_components
                                                msg.0 = command_response.components.unwrap().0;
                                                msg
                                            });
                                        };
            
                                        if command_response.content.is_some() {
                                            message = message.content(command_response.content.unwrap());
                                        };
                        
                                        message
                                    }).await {
                                        warn!("Cannot edit slash command response: {}", why);
                                    };
                                    api_span.finish();
                                },

                                ResponseFallbackMethod::Followup => {
                                    let followup_span = slash_command_tx.start_child("discord.api", "send followup");
                                    if let Err(why) = command.channel_id.send_message(&ctx.http, |mut message| {
                                        // Clone component_response so we can use it in the closure without moving the original data
                                        let command_response = command_response.clone();
                                        if command_response.embed.is_some() {
                                            message = message.set_embed(command_response.embed.unwrap());
                                        };
            
                                        if command_response.components.is_some() {
                                            message = message.components(|msg| {
                                                msg.0 = command_response.components.unwrap().0;
                                                msg
                                            });
                                        };
            
                                        if command_response.content.is_some() {
                                            message = message.content(command_response.content.unwrap());
                                        };
                        
                                        message
                                    }).await {
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

        if let Interaction::MessageComponent(command) = interaction {
            let component_tx = sentry::TransactionOrSpan::from(tx.start_child("interaction.component", "handle component interaction"));
            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().expect("Couldn't get config struct");

            match command.guild_id {
                Some(guild_id) => {
                    info!("{:?} ({:?}) > {:?} ({:?}) : Button {} {:?}", guild_id.name(&ctx.cache).unwrap_or("Name Unavailable".into()), guild_id.as_u64(), command.user.name, command.user.id.as_u64(), command.data.custom_id, command.data.values);
                },
                None => {
                    info!("{:?} ({:?}) : Button {} {:?}", command.user.name, command.user.id.as_u64(), command.data.custom_id, command.data.values);
                }
            }

            // If custom_id uses invalid data structure (old version of bot), ignore interaction
            let custom_id: HashMap<String, serde_json::Value> = if let Ok(custom_id) = serde_json::from_str(&command.data.custom_id) {
                custom_id
            } else {
                return;
            };

            let subreddit = custom_id["subreddit"].to_string().replace('"', "");
            debug!("Search, {:?}", custom_id["search"]);
            let search_enabled = match custom_id["search"] {
                serde_json::Value::String(_) => true,
                _ => false,
            };

            capture_event(data_mut, "subreddit_cmd", Some(&component_tx), Some(HashMap::from([("subreddit", subreddit.clone()), ("button", "true".to_string()), ("search_enabled", search_enabled.to_string())])), &format!("user_{}", command.user.id.0.to_string())).await;
            
            let mut con = match data_mut.get_mut("redis_connection").expect("Couldn't get redis connection") {
                ConfigValue::REDIS(db) => Ok(db),
                _ => Err(0),
            }.expect("Redis connection wasn't a redis connection");

            let component_response = match search_enabled {
                true => {
                    let search = custom_id["search"].to_string().replace('"', "");
                    get_subreddit_search(subreddit, search, &mut con, command.channel_id, Some(&component_tx)).await
                },
                false => {
                    get_subreddit(subreddit, &mut con, command.channel_id, Some(&component_tx)).await
                }
            };

            let component_response = match component_response {
                Ok(component_response) => component_response,
                Err(error) => {
                    let why = format!("{:?}", error);
                    let code = rand::thread_rng().gen_range(0..10000);
                    error!("Error code {} getting command response: {:?}", code, why);

                    let data_mut = data.get_mut::<ConfigStruct>().expect("Couldn't get config struct");

                    capture_event(data_mut, "command_error", Some(&component_tx), Some(HashMap::from([("shard_id", ctx.shard_id.to_string())])), &format!("user_{}", command.user.id)).await;
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
                let to_return = command
                .create_interaction_response(&ctx.http, |response| {
                    response
                        .kind(InteractionResponseType::ChannelMessageWithSource)
                        .interaction_response_data(|mut message| {
                            // Clone component_response so we can use it in the closure without moving the original data
                            let component_response = component_response.clone();
                            if component_response.embed.is_some() {
                                message = message.set_embed(component_response.embed.unwrap());
                            };

                            if component_response.components.is_some() {
                                message = message.set_components(component_response.components.unwrap());
                            };

                            if component_response.content.is_some() {
                                message = message.content(component_response.content.unwrap());
                            };

                            message = message.ephemeral(component_response.ephemeral);

                            message
                        });
                    response
                })
                .await;
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
                                    if let Err(why) = command.edit_original_interaction_response(&ctx.http, |mut message| {
                                        // Clone component_response so we can use it in the closure without moving the original data
                                        let component_response = component_response.clone();
                                        if component_response.embed.is_some() {
                                            message = message.set_embed(component_response.embed.unwrap());
                                        };
            
                                        if component_response.components.is_some() {
                                            message = message.components(|msg| {
                                                msg.0 = component_response.components.unwrap().0;
                                                msg
                                            });
                                        };
            
                                        if component_response.content.is_some() {
                                            message = message.content(component_response.content.unwrap());
                                        };
                        
                                        message
                                    }).await {
                                        warn!("Cannot edit slash command response: {}", why);
                                    };
                                    api_span.finish();
                                },

                                ResponseFallbackMethod::Followup => {
                                    let followup_span = component_tx.start_child("discord.api", "send followup");
                                    if let Err(why) = command.channel_id.send_message(&ctx.http, |mut message| {
                                        // Clone component_response so we can use it in the closure without moving the original data
                                        let component_response = component_response.clone();
                                        if component_response.embed.is_some() {
                                            message = message.set_embed(component_response.embed.unwrap());
                                        };
            
                                        if component_response.components.is_some() {
                                            message = message.components(|msg| {
                                                msg.0 = component_response.components.unwrap().0;
                                                msg
                                            });
                                        };
            
                                        if component_response.content.is_some() {
                                            message = message.content(component_response.content.unwrap());
                                        };
                        
                                        message
                                    }).await {
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
        }
        tx.finish();
    }
}


async fn monitor_total_shards(shard_manager: Arc<Mutex<serenity::client::bridge::gateway::ShardManager>>, total_shards: u64) {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client.get_tokio_connection().await.expect("Can't connect to redis");

    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");

    loop {
        let _ = sleep(Duration::from_secs(60)).await;

        let db_total_shards: redis::RedisResult<u64> = con.get(format!("total_shards_{}", get_namespace().await)).await;
        let db_total_shards: u64 = db_total_shards.expect("Failed to get or convert total_shards from Redis");

        let mut shard_manager = shard_manager.lock().await;
        if !shard_manager.has(serenity::client::bridge::gateway::ShardId(shard_id)).await {
            debug!("Shard {} not found, marking self for termination.", shard_id);
            let _ = fs::remove_file("/etc/probes/live");
        }

        if db_total_shards != total_shards {
            debug!("Total shards changed from {} to {}, restarting.", total_shards, db_total_shards);
            shard_manager.set_shards(shard_id, 1, db_total_shards).await;
            shard_manager.initialize().expect("Failed to initialize shard");
        }
    }
}

async fn get_namespace() -> String {
    let namespace= fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
    return namespace;
}

#[tokio::main]
async fn main() {    
    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: String = env::var("HOSTNAME").expect("HOSTNAME not set").parse().expect("Failed to convert HOSTNAME to string");
    let shard_id: u64 = shard_id.replace("discord-shards-", "").parse().expect("unable to convert shard_id to u64");
    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");

    let redis_client = redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap();
    let mut con = redis_client.get_async_connection().await.expect("Can't connect to redis");

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

    let total_shards: redis::RedisResult<u64> = con.get(format!("total_shards_{}", get_namespace().await)).await;
    let total_shards: u64 = total_shards.expect("Failed to get or convert total_shards");

    debug!("Booting with {:?} total shards", total_shards);

    let mut client = serenity::Client::builder(token,  GatewayIntents::non_privileged())
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let contents:HashMap<String, ConfigValue> = HashMap::from_iter([
        ("shard_id".to_string(), ConfigValue::U64(shard_id as u64)),
        ("redis_connection".to_string(), ConfigValue::REDIS(con)),
        ("mongodb_connection".to_string(), ConfigValue::MONGODB(mongodb_client)),
        ("nsfw_subreddits".to_string(), ConfigValue::SubredditList(nsfw_subreddits)),
        ("posthog_client".to_string(), ConfigValue::PosthogClient(posthog_client)),
    ]);

    {
        let mut data = client.data.write().await;
        data.insert::<ConfigStruct>(contents);
    }

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        debug!("Spawning shard monitor thread");
        monitor_total_shards(shard_manager, total_shards).await;
    });

    let thread = tokio::spawn(async move {
        tracing_subscriber::Registry::default()
        .with(sentry::integrations::tracing::layer().event_filter(|md| {
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
            _ => "r-slash".into()
        };

        let _guard = sentry::init(("http://9ebbf7835f99405ebfca243cf5263782@100.67.30.19:9000/3", sentry::ClientOptions {
            release: sentry::release_name!(),
            traces_sample_rate: 1.0,
            before_send: Some(Arc::new(move |mut event| {
                if let Some(msg) = &event.transaction {
                    if msg.contains("serenity") {
                        return None;
                    }
                };

                event.environment = Some(bot_name.clone());
                event.server_name = Some(shard_id.to_string().into());
                Some(event)
            })),
            ..Default::default()
        }));
    
        debug!("Spawning client thread");
        client.start_shard(shard_id, total_shards as u64).await.expect("Failed to start shard");
    });

    // If client thread exits, shard has crashed, so mark self as unhealthy.
    match thread.await {
        Ok(_) => {},
        Err(_) => {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }
}