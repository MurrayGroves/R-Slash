use std::collections::HashMap;

use anyhow::{bail, Context, Error, anyhow};
use log::warn;
use redis::{from_redis_value, AsyncCommands};
use serde_json::json;
use serenity::all::{ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor};
use tracing::{debug, instrument};
use async_recursion::async_recursion;

use crate::{get_epoch_ms, types::{InteractionResponse, ResponseFallbackMethod}};

#[instrument(skip(con, parent_tx))]
pub async fn get_length_of_search_results(search_index: String, search: String, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<u16, anyhow::Error> {
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
pub async fn get_post_at_search_index(search_index: String, search: &str, index: u16, con: &mut redis::aio::MultiplexedConnection, parent_span: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
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


    debug!("Getting post at index {} in search results for {}", index, new_search);

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
pub async fn get_post_at_list_index(list: String, index: u16, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<String, anyhow::Error> {
    debug!("Getting post at index {} in list {}", index, list);

    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_post_at_list_index").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_at_list_index");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut results: Vec<String> = redis::cmd("LRANGE")
        .arg(&list)
        .arg(index)
        .arg(index)
        .query_async(con).await?;

    if results.is_empty() {
        bail!("No results found in list: {} at index: {}", list, index);
    }

    span.finish();
    Ok(results.remove(0))
}


#[instrument(skip(con, parent_tx))]
pub async fn get_post_by_id<'a>(post_id: &str, search: Option<&str>, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_post_by_id").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_by_id");
            sentry::start_transaction(ctx).into()
        }
    };

    debug!("Getting post by ID: {}", post_id);

    let post: HashMap<String, redis::Value> = con.hgetall(&post_id).await?;

    if post.is_empty() {
        bail!("Post not found: {}", post_id);
    }

    let subreddit = post_id.split(":").collect::<Vec<&str>>()[1].to_string();

    let author = from_redis_value::<String>(&post.get("author").context("No author in post")?.clone())?;
    let title = from_redis_value::<String>(&post.get("title").context("No title in post")?.clone())?;
    let url = from_redis_value::<String>(&post.get("url").context("No url in post")?.clone())?;
    let embed_url = from_redis_value::<String>(&post.get("embed_url").context("No embed_url in post")?.clone())?;
    let timestamp = from_redis_value::<i64>(&post.get("timestamp").context("No timestamp in post")?.clone())?;

    if embed_url.starts_with("https://r-slash") && embed_url.ends_with(".mp4") {
        let filename = embed_url.split("/").last().ok_or(anyhow!("No filename found in URL: {}", url))?;
        let title: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        let author: String = url::form_urlencoded::byte_serialize(author.as_bytes()).collect();
        let subreddit: String = url::form_urlencoded::byte_serialize(subreddit.as_bytes()).collect();
        let embed_url = format!("https://r-slash.b-cdn.net/render/{}?title={}%20-%20by%20u/{}%20in%20r/{}&redirect={}", filename, title, author, subreddit, url);
        
        span.finish();
        return Ok(InteractionResponse {
            content: Some(format!("[.]({})", embed_url)),
            embed: None,
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
        });
    }
    
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
#[async_recursion]
pub async fn get_subreddit<'a>(subreddit: String, con: &mut redis::aio::MultiplexedConnection, channel: ChannelId, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<InteractionResponse, anyhow::Error> {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("subreddit.get", "get_subreddit").into(),
        None => {
            let ctx = sentry::TransactionContext::new("subreddit.get", "get_subreddit");
            sentry::start_transaction(ctx).into()
        }
    };

    let subreddit = subreddit.to_lowercase();

    let fetched_posts: HashMap<String, u64> = con.hgetall(format!("subreddit:{}:channels:{}:posts", &subreddit, channel)).await?;
    let posts: Vec<String> = con.lrange(format!("subreddit:{}:posts", &subreddit), 0, -1).await?;

    let mut post_id: Result<String, Error> = Err(anyhow!("No posts found for subreddit: {}", subreddit));

    // Find the first post that the channel has not seen before
    let mut minimum_post: Option<(String, u64)> = None;
    for post in posts {
        if fetched_posts.contains_key(&post) {
            if minimum_post.is_none() || fetched_posts.get(&post).unwrap() < &minimum_post.clone().unwrap().1 {
                minimum_post = Some((post.clone(), fetched_posts.get(&post).unwrap().to_owned()));
            }
        } else {
            post_id = Ok(post);
            break;
        }
    }

    // If all posts have been seen, find the post that the channel saw longest ago
    if post_id.is_err() {
        match minimum_post {
            Some((post, _)) => {
                post_id = Ok(post.to_string());
            },
            None => {}
        };
    }

    let post_id = match post_id {
        Ok(id) => id,
        Err(e) => bail!(e),
    };


    let _:() = con.hset(format!("subreddit:{}:channels:{}:posts", &subreddit, channel), &post_id, get_epoch_ms()).await?;

    let mut post = get_post_by_id(&post_id, None, con, Some(&span)).await;

    // If the post is not found, some bug has occurred, remove the post from the subreddit list and call this function again to get a new one
    if let Err(e) = post {
        warn!("Error getting post by ID: {}", e);
        let _:() = con.lrem(format!("subreddit:{}:posts", &subreddit), 0, &post_id).await?;
        post = get_subreddit(subreddit, con, channel, Some(&span)).await;
    }

    span.finish();
    return post;
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

    let mut search = search.replace('"', "\\");
    search = search.replace(":", "\\:");

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

    let mut post_id = get_post_at_search_index(format!("idx:{}", &subreddit), &search, index, con, Some(&span)).await?;
    let mut post = get_post_by_id(&post_id, Some(&search), con, Some(&span)).await;
    while let Err(_) = post {
        index += 1;
        if index >= length {
            let _:() = con.set(format!("subreddit:{}:search:{}:channels:{}:index", &subreddit, &search, channel), 0i16).await?;
            index = 0;
        }
        post_id = get_post_at_search_index(format!("idx:{}", &subreddit), &search, index, con, Some(&span)).await?;
        post = get_post_by_id(&post_id, Some(&search), con, Some(&span)).await;
    };
    span.finish();
    return post;
}



#[instrument(skip(con, parent_tx))]
pub async fn list_contains(element: &str, list: &str, con: &mut redis::aio::MultiplexedConnection, parent_tx: Option<&sentry::TransactionOrSpan>) -> Result<bool, anyhow::Error> {
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
