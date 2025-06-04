use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, ensure, Context, Error};
use async_recursion::async_recursion;
use log::warn;
use redis::aio::MultiplexedConnection;
use redis::{from_redis_value, AsyncTypedCommands};
use reqwest::header;
use reqwest::header::HeaderMap;
use serde_json::json;
use serenity::all::{
    ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed, CreateEmbedAuthor,
    GenericChannelId,
};
use serenity::builder::{
    CreateComponent, CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
};
use tokio::time::sleep;
use tracing::{debug, error, error_span, instrument};

use rslash_common::{InteractionResponse, InteractionResponseMessage, ResponseFallbackMethod};

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

#[derive(Debug)]
pub enum PostApiError {
    NoPostsFound { subreddit: String },
    PostNotFoundInList { list: String, index: u16 },
    PostNotFound { post_id: String },
}

impl std::fmt::Display for PostApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            PostApiError::NoPostsFound { subreddit } => {
                write!(f, "No posts found in subreddit: {}", subreddit)
            }
            PostApiError::PostNotFoundInList { list, index } => {
                write!(f, "No post found in list: {} at index: {}", list, index)
            }
            PostApiError::PostNotFound { post_id } => {
                write!(f, "Post not found: {}", post_id)
            }
        }
    }
}

#[instrument(skip(con))]
pub async fn get_length_of_search_results(
    search_index: String,
    search: &str,
    con: &mut redis::aio::MultiplexedConnection,
) -> Result<isize, anyhow::Error> {
    let new_search = format!("w'*{}*'", redis_sanitise(&search));

    debug!(
        "Getting length of search results for {} in {}",
        new_search, search_index
    );
    let results: Vec<isize> = redis::cmd("FT.SEARCH")
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
    index: isize,
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

    ensure!(
        !results.is_empty(),
        PostApiError::PostNotFoundInList { list, index }
    );

    span.finish();
    Ok(results.remove(0))
}

/// Represents a fetched Reddit post including some context about how it was fetched
#[derive(Debug)]
pub struct Post {
    pub subreddit: String,
    pub author: String,
    pub title: String,
    /// The URL of the post on Reddit
    pub url: String,
    /// The converted embed URL
    pub embed_url: String,
    /// The timestamp when the post was made on Reddit
    pub timestamp: serenity::model::timestamp::Timestamp,
    /// The search term used to fetch this post, if any
    pub search: Option<String>,
}

impl Post {
    fn get_components<'a, 'b>(&'a self) -> Vec<CreateComponent<'b>> {
        vec![CreateComponent::ActionRow(CreateActionRow::Buttons(
            Cow::from(vec![
                CreateButton::new(
                    json!({
                        "subreddit": self.subreddit,
                        "search": self.search.clone().unwrap_or("".to_string()),
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
            ]),
        ))]
    }
}

impl<'a> Into<CreateInteractionResponse<'a>> for Post {
    /// Includes buttons
    fn into(self) -> CreateInteractionResponse<'a> {
        if self.embed_url.starts_with("https://cdn.rsla.sh") && self.embed_url.ends_with(".mp4") {
            let msg = CreateInteractionResponseMessage::default()
                .content(format!("[.]({})", self.embed_url))
                .components(self.get_components());

            CreateInteractionResponse::Message(msg)
        } else {
            let msg = CreateInteractionResponseMessage::default()
                .embed(
                    CreateEmbed::default()
                        .title(self.title.clone())
                        .description(format!("r/{}", self.subreddit))
                        .author(
                            CreateEmbedAuthor::new(format!("u/{}", self.author))
                                .url(format!("https://reddit.com/u/{}", self.author)),
                        )
                        .url(self.url.clone())
                        .color(0x00ff00)
                        .image(self.embed_url.clone())
                        .timestamp(self.timestamp)
                        .to_owned(),
                )
                .components(self.get_components());

            CreateInteractionResponse::Message(msg)
        }
    }
}

impl<'a> Into<CreateMessage<'a>> for Post {
    fn into(self) -> CreateMessage<'a> {
        if self.embed_url.starts_with("https://cdn.rsla.sh") && self.embed_url.ends_with(".mp4") {
            CreateMessage::new()
                .content(format!("[.]({})", self.embed_url))
                .components(self.get_components())
        } else {
            CreateMessage::new()
                .embed(
                    CreateEmbed::default()
                        .title(self.title.clone())
                        .description(format!("r/{}", self.subreddit))
                        .author(
                            CreateEmbedAuthor::new(format!("u/{}", self.author))
                                .url(format!("https://reddit.com/u/{}", self.author)),
                        )
                        .url(self.url.clone())
                        .color(0x00ff00)
                        .image(self.embed_url.clone())
                        .timestamp(self.timestamp)
                        .to_owned(),
                )
                .components(self.get_components())
        }
    }
}

pub fn optional_post_to_response(
    post: Option<Post>,
    buttons: bool,
) -> CreateInteractionResponse<'static> {
    match post {
        Some(post) => post.into(),
        None =>                         serenity::builder::CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(
            CreateEmbed::default()
                .title("No Posts Found")
                .description("No supported posts found in this subreddit. Try searching for something else.")
                .color(0xff0000)
                .to_owned(),)
        ),
    }
}

pub fn optional_post_to_message(post: Option<Post>, buttons: bool) -> CreateMessage<'static> {
    match post {
        Some(post) => post.into(),
        None => CreateMessage::new().embed(
            CreateEmbed::default()
                .title("No Posts Found")
                .description(
                    "No supported posts found in this subreddit. Try searching for something else.",
                )
                .color(0xff0000)
                .to_owned(),
        ),
    }
}

#[instrument(skip(con))]
pub async fn get_post_by_id<'a>(
    post_id: &str,
    search: Option<&str>,
    con: &mut redis::aio::MultiplexedConnection,
    add_buttons: bool,
) -> Result<Post, anyhow::Error> {
    debug!("Getting post by ID: {}", post_id);

    let post: HashMap<String, String> = con.hgetall(&post_id).await?;

    ensure!(
        !post.is_empty(),
        PostApiError::PostNotFound {
            post_id: post_id.to_string()
        }
    );

    let subreddit = post_id.split(":").collect::<Vec<&str>>()[1].to_string();

    let author = post.get("author").context("No author in post")?.clone();
    let title = post.get("title").context("No title in post")?.clone();
    let url = post.get("url").context("No url in post")?.clone();
    let mut embed_url: String = post
        .get("embed_url")
        .context("No embed_url in post")?
        .clone();
    let timestamp = post
        .get("timestamp")
        .context("No timestamp in post")?
        .parse()?;

    if !embed_url.starts_with("http") {
        embed_url = format!("https://cdn.rsla.sh/gifs/{}", embed_url);
    }

    if (embed_url.starts_with("https://r-slash.b-cdn.net")
        || embed_url.starts_with("https://cdn.rsla.sh"))
        && embed_url.ends_with(".mp4")
    {
        let filename = embed_url
            .split("/")
            .last()
            .ok_or(anyhow!("No filename found in URL: {}", url))?;
        let title: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        let author: String = url::form_urlencoded::byte_serialize(author.as_bytes()).collect();
        let subreddit: String =
            url::form_urlencoded::byte_serialize(subreddit.as_bytes()).collect();
        embed_url = format!(
            "https://cdn.rsla.sh/render/{}?title={}%20-%20by%20u/{}%20in%20r/{}&redirect={}",
            filename, title, author, subreddit, url
        );
    }

    Ok(Post {
        subreddit,
        author,
        title,
        url,
        embed_url,
        timestamp: serenity::model::timestamp::Timestamp::from_unix_timestamp(timestamp)?,
        search: search.map(|s| s.to_string()),
    })
}

#[instrument(skip(con))]
#[async_recursion]
pub async fn get_subreddit<'a>(
    subreddit: &str,
    con: &mut redis::aio::MultiplexedConnection,
    channel: GenericChannelId,
) -> Result<Post, anyhow::Error> {
    let subreddit = subreddit.to_lowercase();

    let fetched_posts: HashMap<String, u64> = redis::AsyncCommands::hgetall(
        con,
        format!("subreddit:{}:channels:{}:posts", &subreddit, channel),
    )
    .await?;
    let posts: Vec<String> = con
        .lrange(format!("subreddit:{}:posts", &subreddit), 0, -1)
        .await?;

    // Find the first post that the channel has not seen before
    let mut post_id: Option<String> = None;
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
            post_id = Some(post);
            break;
        }
    }

    // If all posts have been seen, find the post that the channel saw longest ago
    if post_id.is_none() {
        match minimum_post {
            Some((post, _)) => {
                post_id = Some(post.to_string());
            }
            None => {}
        };
    }

    let post_id = match post_id {
        Some(id) => id,
        None => bail!(PostApiError::NoPostsFound { subreddit }),
    };

    con.hset(
        format!("subreddit:{}:channels:{}:posts", &subreddit, channel),
        &post_id,
        get_epoch_ms(),
    )
    .await?;

    let mut post = get_post_by_id(&post_id, None, con, true).await;

    // If the post is not found, some bug has occurred, remove the post from the subreddit list and call this function again to get a new one
    if let Err(e) = post {
        let span = error_span!("delete_post_from_list");
        {
            let _ = span.enter();
            error!("Error getting post by ID: {:?}", e);
            con.lrem(format!("subreddit:{}:posts", &subreddit), 1, &post_id)
                .await?;
        }
        post = get_subreddit(&subreddit, con, channel).await;
    }

    post
}

/// Gets a post from a subreddit by searching for a term. Returns `None` if no post is found matching search.
#[instrument(skip(con))]
pub async fn get_subreddit_search<'a>(
    subreddit: &str,
    search: &str,
    con: &mut redis::aio::MultiplexedConnection,
    channel: GenericChannelId,
) -> Result<Option<Post>, anyhow::Error> {
    debug!(
        "Getting post for search: {} in subreddit: {}",
        search, subreddit
    );

    let mut index = con
        .incr(
            format!(
                "subreddit:{}:search:{}:channels:{}:index",
                &subreddit, &search, channel
            ),
            1i16,
        )
        .await?;

    con.expire(
        format!(
            "subreddit:{}:search:{}:channels:{}:index",
            &subreddit, &search, channel
        ),
        60 * 60,
    )
    .await?;
    index -= 1;
    let length = get_length_of_search_results(format!("idx:{}", &subreddit), &search, con).await?;

    if length == 0 {
        return Ok(None);
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
    let mut post = get_post_by_id(&post_id, Some(&search), con, true).await;
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
        post = get_post_by_id(&post_id, Some(&search), con, true).await;
    }

    Ok(Some(post?))
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
    let mut default_headers = HeaderMap::new();
    default_headers.insert(
        header::COOKIE,
        header::HeaderValue::from_static("_options={%22pref_gated_sr_optin%22:true}"),
    );

    debug!("Checking subreddit validity: {}", subreddit);

    let web_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers(default_headers)
        .user_agent(format!(
            "Discord:RSlash:{} (by /u/murrax2)",
            env!("CARGO_PKG_VERSION")
        ))
        .build()?;
    let res = web_client
        .head(format!("https://www.reddit.com/r/{}.json", subreddit))
        .send()
        .await?;

    Ok(if res.status() == 200 {
        SubredditStatus::Valid
    } else {
        SubredditStatus::Invalid(res.text().await?)
    })
}

pub async fn queue_subreddit(
    subreddit: &str,
    con: &mut MultiplexedConnection,
    bot: u64,
) -> Result<(), Error> {
    let already_queued = list_contains(&subreddit, "custom_subreddits_queue", con).await?;

    let last_cached = con.get_int(&format!("{}", subreddit)).await?.unwrap_or(0) as u64;

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

            let last_cached = con.get_int(&format!("{}", subreddit)).await?.unwrap_or(0);

            if last_cached != 0 {
                break;
            }

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
    } else if last_cached + 3600000 < get_epoch_ms() {
        debug!("Subreddit last cached more than an hour ago, updating...");
        // Tell downloader to update the subreddit, but use outdated posts for now.
        if !already_queued {
            con.rpush("custom_subreddits_queue", &subreddit).await?;
        }
    }

    Ok(())
}
