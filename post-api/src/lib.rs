use std::collections::HashMap;
use std::thread::current;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Error};
use async_recursion::async_recursion;
use log::warn;
use redis::aio::MultiplexedConnection;
use redis::{from_redis_value, AsyncCommands};
use serde_json::json;
use serenity::all::{
    ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor,
};
use tokio::time::sleep;
use tracing::{debug, instrument};

use rslash_types::{InteractionResponse, InteractionResponseMessage, ResponseFallbackMethod};

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn redis_sanitise(input: &str) -> String {
    let special = vec![
        ",", ".", "<", ">", "{", "}", "[", "]", "\"", "'", ":", ";", "!", "@", "#", "$", "%", "^",
        "&", "*", "(", ")", "-", "+", "=", "~",
    ];

    let mut output = input.to_string();
    for s in special {
        output = output.replace(s, &("\\".to_owned() + s));
    }

    output = output.replace("\n", " ");

    output
}

#[instrument(skip(con))]
pub async fn get_length_of_search_results(
    search_index: String,
    search: &str,
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<u16, anyhow::Error> {
    let new_search = format!("w'*{}*'", redis_sanitise(&search));

    debug!(
        "Getting length of search results for {} in {}",
        new_search, search_index
    );
    let results: Vec<u16> = redis::cmd("FT.SEARCH")
        .arg(search_index)
        .arg(&new_search)
        .arg("LIMIT")
        .arg(0) // Return no results, just number of results
        .arg(0)
        .arg("DIALECT")
        .arg(2)
        .query_async(con)
        .await?;

    Ok(results[0])
}

// Returns the post ID at the given index in the search results
#[instrument(skip(con))]
pub async fn get_post_at_search_index(
    search_index: String,
    search: &str,
    index: u16,
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<String, anyhow::Error> {
    let new_search = format!("w'*{}*'", redis_sanitise(&search));

    debug!(
        "Getting post at index {} in search results for {}",
        index, new_search
    );

    let results: Vec<redis::Value> = redis::cmd("FT.SEARCH")
        .arg(search_index)
        .arg(&new_search)
        .arg("LIMIT")
        .arg(index)
        .arg(1)
        .arg("SORTBY")
        .arg("score")
        .arg("NOCONTENT") // Only show POST IDs not post content
        .arg("DIALECT")
        .arg(2)
        .query_async(con)
        .await?;

    Ok(from_redis_value::<String>(&results[1])?)
}

// Returns the post ID at the given index in the list
#[instrument(skip(con, parent_tx))]
pub async fn get_post_at_list_index(
    list: String,
    index: u16,
    con: &mut redis::aio::MultiplexedConnection,
    parent_tx: Option<&sentry::TransactionOrSpan>,
) -> Result<String, anyhow::Error> {
    debug!("Getting post at index {} in list {}", index, list);

    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent
            .start_child("db.query", "get_post_at_list_index")
            .into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_post_at_list_index");
            sentry::start_transaction(ctx).into()
        }
    };

    let mut results: Vec<String> = redis::cmd("LRANGE")
        .arg(&list)
        .arg(index)
        .arg(index)
        .query_async(con)
        .await?;

    if results.is_empty() {
        bail!("No results found in list: {} at index: {}", list, index);
    }

    span.finish();
    Ok(results.remove(0))
}

#[instrument(skip(con))]
pub async fn get_post_by_id<'a>(
    post_id: &str,
    search: Option<&str>,
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<InteractionResponse, anyhow::Error> {
    debug!("Getting post by ID: {}", post_id);

    let post: HashMap<String, redis::Value> = con.hgetall(&post_id).await?;

    if post.is_empty() {
        bail!("Post not found: {}", post_id);
    }

    let subreddit = post_id.split(":").collect::<Vec<&str>>()[1].to_string();

    let author = from_redis_value::<String>(post.get("author").context("No author in post")?)?;
    let title = from_redis_value::<String>(post.get("title").context("No title in post")?)?;
    let url = from_redis_value::<String>(post.get("url").context("No url in post")?)?;
    let embed_url =
        from_redis_value::<String>(post.get("embed_url").context("No embed_url in post")?)?;
    let timestamp =
        from_redis_value::<i64>(post.get("timestamp").context("No timestamp in post")?)?;

    if embed_url.starts_with("https://r-slash") && embed_url.ends_with(".mp4") {
        let filename = embed_url
            .split("/")
            .last()
            .ok_or(anyhow!("No filename found in URL: {}", url))?;
        let title: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        let author: String = url::form_urlencoded::byte_serialize(author.as_bytes()).collect();
        let subreddit: String =
            url::form_urlencoded::byte_serialize(subreddit.as_bytes()).collect();
        let embed_url = format!(
            "https://r-slash.b-cdn.net/render/{}?title={}%20-%20by%20u/{}%20in%20r/{}&redirect={}",
            filename, title, author, subreddit, url
        );

        return Ok(InteractionResponse::Message(InteractionResponseMessage {
            content: Some(format!("[.]({})", embed_url)),
            embed: None,
            components: Some(vec![CreateActionRow::Buttons(vec![
                CreateButton::new(
                    json!({
                        "subreddit": subreddit,
                        "search": search,
                        "command": "again"
                    })
                    .to_string(),
                )
                .label("🔁")
                .style(ButtonStyle::Primary),
                CreateButton::new(
                    json!({
                        "command": "where-autopost"
                    })
                    .to_string(),
                )
                .label("Where's the auto-post button?")
                .style(ButtonStyle::Secondary),
            ])]),

            fallback: ResponseFallbackMethod::Edit,
            ..Default::default()
        }));
    }

    let to_return = InteractionResponse::Message(InteractionResponseMessage {
        embed: Some(
            CreateEmbed::default()
                .title(title)
                .description(format!("r/{}", subreddit))
                .author(
                    CreateEmbedAuthor::new(format!("u/{}", author))
                        .url(format!("https://reddit.com/u/{}", author)),
                )
                .url(url)
                .color(0x00ff00)
                .image(embed_url)
                .timestamp(serenity::model::timestamp::Timestamp::from_unix_timestamp(
                    timestamp,
                )?)
                .to_owned(),
        ),

        components: Some(vec![CreateActionRow::Buttons(vec![
            CreateButton::new(
                json!({
                    "subreddit": subreddit,
                    "search": search,
                    "command": "again"
                })
                .to_string(),
            )
            .label("🔁")
            .style(ButtonStyle::Primary),
            CreateButton::new(
                json!({
                    "command": "where-autopost"
                })
                .to_string(),
            )
            .label("Where's the auto-post button?")
            .style(ButtonStyle::Secondary),
        ])]),

        fallback: ResponseFallbackMethod::Edit,
        ..Default::default()
    });

    return Ok(to_return);
}

#[instrument(skip(con))]
#[async_recursion]
pub async fn get_subreddit<'a>(
    subreddit: String,
    con: &mut redis::aio::MultiplexedConnection,
    channel: ChannelId,
) -> Result<InteractionResponse, anyhow::Error> {
    let subreddit = subreddit.to_lowercase();

    let fetched_posts: HashMap<String, u64> = con
        .hgetall(format!(
            "subreddit:{}:channels:{}:posts",
            &subreddit, channel
        ))
        .await?;
    let posts: Vec<String> = con
        .lrange(format!("subreddit:{}:posts", &subreddit), 0, -1)
        .await?;

    let mut post_id: Result<String, Error> =
        Err(anyhow!("No posts found for subreddit: {}", subreddit));

    // Find the first post that the channel has not seen before
    let mut minimum_post: Option<(String, u64)> = None;
    for post in posts.into_iter() {
        if fetched_posts.contains_key(&post) {
            let timestamp = *fetched_posts.get(&post).unwrap();
            if let Some(current_min) = &minimum_post {
                if timestamp < current_min.1 {
                    minimum_post = Some((post, timestamp));
                }
            } else {
                minimum_post = Some((post, timestamp));
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
            }
            None => {}
        };
    }

    let post_id = match post_id {
        Ok(id) => id,
        Err(e) => bail!(e),
    };

    let _: () = con
        .hset(
            format!("subreddit:{}:channels:{}:posts", &subreddit, channel),
            &post_id,
            get_epoch_ms(),
        )
        .await?;

    let mut post = get_post_by_id(&post_id, None, con).await;

    // If the post is not found, some bug has occurred, remove the post from the subreddit list and call this function again to get a new one
    if let Err(e) = post {
        warn!("Error getting post by ID: {}", e);
        let _: () = con
            .lrem(format!("subreddit:{}:posts", &subreddit), 0, &post_id)
            .await?;
        post = get_subreddit(subreddit, con, channel).await;
    }

    return post;
}

#[instrument(skip(con))]
pub async fn get_subreddit_search<'a>(
    subreddit: String,
    search: String,
    con: &mut redis::aio::MultiplexedConnection,
    channel: ChannelId,
) -> Result<InteractionResponse, anyhow::Error> {
    debug!(
        "Getting post for search: {} in subreddit: {}",
        search, subreddit
    );

    let mut index: u16 = con
        .incr(
            format!(
                "subreddit:{}:search:{}:channels:{}:index",
                &subreddit, &search, channel
            ),
            1i16,
        )
        .await?;
    let _: () = con
        .expire(
            format!(
                "subreddit:{}:search:{}:channels:{}:index",
                &subreddit, &search, channel
            ),
            60 * 60,
        )
        .await?;
    index -= 1;
    let length: u16 =
        get_length_of_search_results(format!("idx:{}", &subreddit), &search, con).await?;

    if length == 0 {
        return Ok(InteractionResponse::Message(InteractionResponseMessage {
            embed: Some(
                CreateEmbed::default()
                    .title("No search results found")
                    .color(0xff0000)
                    .to_owned(),
            ),
            ..Default::default()
        }));
    }

    index = length - (index + 1);

    if index >= length {
        let _: () = con
            .set(
                format!(
                    "subreddit:{}:search:{}:channels:{}:index",
                    &subreddit, &search, channel
                ),
                0i16,
            )
            .await?;
        index = 0;
    }

    let mut post_id =
        get_post_at_search_index(format!("idx:{}", &subreddit), &search, index, con).await?;
    let mut post = get_post_by_id(&post_id, Some(&search), con).await;
    while let Err(_) = post {
        index += 1;
        if index >= length {
            let _: () = con
                .set(
                    format!(
                        "subreddit:{}:search:{}:channels:{}:index",
                        &subreddit, &search, channel
                    ),
                    0i16,
                )
                .await?;
            index = 0;
        }
        post_id =
            get_post_at_search_index(format!("idx:{}", &subreddit), &search, index, con).await?;
        post = get_post_by_id(&post_id, Some(&search), con).await;
    }

    return post;
}

#[instrument(skip(con))]
pub async fn list_contains(
    element: &str,
    list: &str,
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<bool, anyhow::Error> {
    let position: Option<u16> = con
        .lpos(list, element, redis::LposOptions::default())
        .await?;

    let to_return = match position {
        Some(_) => Ok(true),
        None => Ok(false),
    };

    to_return
}

pub enum SubredditStatus {
    Valid,
    Invalid(String),
}

pub async fn check_subreddit_valid(subreddit: &str) -> Result<SubredditStatus, Error> {
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

    return Ok(if res.status() == 200 {
        SubredditStatus::Valid
    } else {
        SubredditStatus::Invalid(res.text().await?)
    });
}

pub async fn queue_subreddit(
    subreddit: &str,
    con: &mut MultiplexedConnection,
    bot: u64,
) -> Result<(), Error> {
    let already_queued = list_contains(&subreddit, "custom_subreddits_queue", con).await?;

    let last_cached: i64 = con.get(&format!("{}", subreddit)).await.unwrap_or(0);

    if last_cached == 0 {
        debug!("Subreddit not cached");

        if !already_queued {
            debug!("Queueing subreddit for download");
            con.rpush("custom_subreddits_queue", &subreddit).await?;

            let selector = if bot == 278550142356029441 {
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
                .arg(format!("subreddit:{}:posts", subreddit))
                .arg(0i64)
                .arg(0i64)
                .query_async(con)
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

    Ok(())
}
