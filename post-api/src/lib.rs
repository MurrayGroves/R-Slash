#![feature(round_char_boundary)]

use chrono::format::parse;
use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Error, bail, ensure};
use async_recursion::async_recursion;
use ellipse::Ellipse;
use indoc::indoc;
use metrics::counter;
use redis::aio::MultiplexedConnection;
use redis::{AsyncTypedCommands, from_redis_value};
use rslash_common::access_tokens::get_reddit_access_token;
use rslash_common::{Post, SubredditStatus, get_post_content_type};
use serde_json::json;
use serenity::all::{
    ButtonStyle, ChannelId, CreateActionRow, CreateButton, CreateEmbed,
    CreateInteractionResponseFollowup, CreateMediaGallery, CreateSeparator, GenericChannelId,
    MessageFlags,
};
use serenity::builder::{
    CreateComponent, CreateContainer, CreateInteractionResponse, CreateInteractionResponseMessage,
    CreateMediaGalleryItem, CreateMessage, CreateTextDisplay, CreateUnfurledMediaItem,
};
use tokio::time::sleep;
use tracing::{debug, error, error_span, info, instrument};
use url::Url;
use user_config_manager::get_channel_config;

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
    con: &mut MultiplexedConnection,
) -> Result<isize, Error> {
    let new_search = format!(
        "%{}%",
        redis_sanitise(&search)
            .split(" ")
            .collect::<Vec<_>>()
            .join("% %")
    );

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
    con: &mut MultiplexedConnection,
) -> Result<String, Error> {
    let new_search = format!(
        "%{}%",
        redis_sanitise(&search)
            .split(" ")
            .collect::<Vec<_>>()
            .join("% %")
    );

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
        .arg("DESC")
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
    con: &mut MultiplexedConnection,
    parent_tx: Option<&sentry::TransactionOrSpan>,
) -> Result<String, Error> {
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
pub struct PostInContext {
    pub subreddit: String,
    /// The search term that was used to fetch this post, if any
    pub search: Option<String>,
    pub post: Post,
}

fn pretty_number(num: isize) -> String {
    if num >= 1_000 {
        format!("{:.1}k", num as f64 / 1_000.0)
    } else {
        num.to_string()
    }
}

impl PostInContext {
    fn get_components<'a>(self, include_buttons: bool) -> Vec<CreateComponent<'a>> {
        let mut container = Vec::new();
        container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
            format!(
                indoc! {"
                    ## [{}]({})
                    by [u/{}](https://reddit.com/u/{}) in [r/{}](https://reddit.com/r/{})
                "},
                self.post.title,
                self.post.url,
                self.post.author,
                self.post.author,
                self.subreddit,
                self.subreddit
            ),
        )));

        container.push(CreateComponent::Separator(CreateSeparator::new(true)));

        if !self.post.embed_urls.is_empty() && self.post.embed_urls.first().unwrap() != "" {
            container.push(CreateComponent::MediaGallery(CreateMediaGallery::new(
                self.post
                    .embed_urls
                    .into_iter()
                    .take(10) // Media gallery can only show 10 items
                    .map(|url| CreateMediaGalleryItem::new(CreateUnfurledMediaItem::new(url)))
                    .collect::<Vec<_>>(),
            )));
        }

        let mut text_chars = self.post.title.len() + self.post.author.len() + self.subreddit.len();

        if let Some(title) = self.post.linked_url_title {
            let title = html_escape::decode_html_entities(&title);
            if title != self.post.title {
                // No point putting it twice
                container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
                    format!("### [{}]({})", title, self.post.linked_url.clone().unwrap()),
                )));
                text_chars += title.len();
            }
        }

        if let Some(mut url) = self.post.linked_url_image.clone() {
            if url.starts_with("//") {
                // Deal with protocol relative URLs
                url = format!("https:{}", url);
            };
            if let Ok(mut url) = Url::parse(&url) {
                url.set_query(None);
                container.push(CreateComponent::MediaGallery(CreateMediaGallery::new(
                    vec![CreateMediaGalleryItem::new(CreateUnfurledMediaItem::new(
                        url.as_str().to_string(),
                    ))],
                )))
            }
        }

        if let Some(description) = self.post.linked_url_description {
            let description = html_escape::decode_html_entities(&description).to_string();
            text_chars += description.len();
            container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
                description,
            )));
        }

        if let Some(url) = self.post.linked_url {
            if let Ok(url_obj) = Url::parse(&url)
                && let Some(domain) = url_obj.domain()
            {
                container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
                    format!("-# [{}]({})", domain, url),
                )))
            } else {
                debug!("Failed to parse URL: {:?}", url);
            }
        }

        if let Some(text) = self.post.text {
            container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
                text.as_str()
                    .truncate_ellipse_with(3500 - text_chars, " ...")
                    .to_string(),
            ))); // Truncate to fit in Discord char limit
        }

        container.push(CreateComponent::TextDisplay(CreateTextDisplay::new(
            format!(
                "*Posted <t:{}:R>, currently has {} points*",
                self.post.timestamp,
                pretty_number(self.post.score)
            ),
        )));

        let mut components = vec![CreateComponent::Container(CreateContainer::new(container))];

        if include_buttons {
            components.push(CreateComponent::ActionRow(CreateActionRow::Buttons(
                Cow::from(vec![
                    CreateButton::new(
                        json!({
                            "subreddit": self.subreddit,
                            "search": self.search,
                            "command": "again"
                        })
                        .to_string(),
                    )
                    .label("🔁")
                    .style(ButtonStyle::Primary),
                ]),
            )))
        }

        components
    }

    pub fn buttonless_message<'a>(self) -> CreateMessage<'a> {
        CreateMessage::new()
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(self.get_components(false))
    }
}

impl<'a> Into<CreateInteractionResponse<'a>> for PostInContext {
    /// Includes buttons
    fn into(self) -> CreateInteractionResponse<'a> {
        CreateInteractionResponse::Message(self.into())
    }
}

impl<'a> Into<CreateInteractionResponseMessage<'a>> for PostInContext {
    fn into(self) -> CreateInteractionResponseMessage<'a> {
        CreateInteractionResponseMessage::default()
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(self.get_components(true))
    }
}

impl<'a> Into<CreateInteractionResponseFollowup<'a>> for PostInContext {
    /// Includes buttons
    fn into(self) -> CreateInteractionResponseFollowup<'a> {
        CreateInteractionResponseFollowup::default()
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(self.get_components(true))
    }
}

impl<'a> Into<CreateMessage<'a>> for PostInContext {
    fn into(self) -> CreateMessage<'a> {
        CreateMessage::new()
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(self.get_components(true))
    }
}

pub fn optional_post_to_response(
    post: Option<PostInContext>,
) -> CreateInteractionResponse<'static> {
    match post {
		Some(post) => post.into(),
		None => CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(
			CreateEmbed::default()
				.title("No Posts Found")
				.description("No supported posts found in this subreddit. Try searching for something else.")
				.color(0xff0000)
				.to_owned(), )
		),
	}
}

pub fn optional_post_to_message(
    post: Option<PostInContext>,
    buttons: bool,
) -> CreateMessage<'static> {
    match post {
        Some(post) => {
            if buttons {
                post.into()
            } else {
                post.buttonless_message()
            }
        }
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

/// Gets a post by ID of form `subreddit:{subreddit}:post:{reddit post ID}`
#[instrument(skip(con))]
pub async fn get_post_by_id<'a>(
    post_id: &str,
    search: Option<&str>,
    con: &mut MultiplexedConnection,
) -> Result<PostInContext, Error> {
    counter!("postapi_fetched_posts").increment(1);
    let post_id = post_id.to_lowercase();
    debug!("Getting post by ID: {}", post_id);

    let post: Post = redis::AsyncCommands::hgetall(con, &post_id).await?;

    Ok(PostInContext {
        subreddit: post_id.split(':').nth(1).unwrap_or("").to_string(),
        search: search.map(|s| s.to_string()),
        post,
    })
}

#[instrument(skip(redis, mongodb))]
#[async_recursion]
pub async fn get_subreddit<'a>(
    subreddit: &str,
    redis: &mut MultiplexedConnection,
    mongodb: &mut mongodb::Client,
    channel: GenericChannelId,
    recursion_level: Option<u8>,
) -> Result<PostInContext, Error> {
    let subreddit = subreddit.to_lowercase();

    let fetched_posts: HashMap<String, u64> = redis::AsyncCommands::hgetall(
        redis,
        format!("subreddit:{}:channels:{}:posts", &subreddit, channel),
    )
    .await?;

    let posts: Vec<String> = redis
        .lrange(format!("subreddit:{}:posts", &subreddit), 0, -1)
        .await?;

    let text_allow_level = get_channel_config(mongodb, ChannelId::new(channel.get()))
        .await?
        .text_allowed
        .unwrap_or_default();

    debug!("Text allow level is {:?}", text_allow_level);

    // Find the first post that the channel has not seen before
    let mut post_id: Option<String> = None;
    let mut minimum_post: Option<(String, u64)> = None;
    for post in posts.into_iter() {
        if fetched_posts.contains_key(&post) {
            let timestamp = *fetched_posts.get(&post).unwrap();
            if let Some(current_min) = &minimum_post {
                if timestamp < current_min.1 {
                    let content_type = get_post_content_type(redis, &post).await?;
                    if text_allow_level.allows_for(content_type) {
                        minimum_post = Some((post, timestamp));
                    }
                }
            } else {
                let content_type = get_post_content_type(redis, &post).await?;
                if text_allow_level.allows_for(content_type) {
                    minimum_post = Some((post, timestamp));
                }
            }
        } else {
            let content_type = get_post_content_type(redis, &post).await?;
            if text_allow_level.allows_for(content_type) {
                post_id = Some(post);
                break;
            }
        }
    }

    // If all posts have been seen, find the post that the channel saw longest ago
    if post_id.is_none() {
        debug!("Channel has seen all posts, using the oldest post");
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

    redis
        .hset(
            format!("subreddit:{}:channels:{}:posts", &subreddit, channel),
            &post_id,
            get_epoch_ms(),
        )
        .await?;

    let mut post = get_post_by_id(&post_id, None, redis).await;

    // If the post is not found, some bug has occurred, remove the post from the subreddit list and call this function again to get a new one
    if let Err(e) = post {
        let span = error_span!("delete_post_from_list");
        {
            let _ = span.enter();
            info!("Error was: {:?}, getting {:?}", e, post_id);
            error!("Error getting post by ID");
            redis
                .lrem(format!("subreddit:{}:posts", &subreddit), 1, &post_id)
                .await?;
        }

        if let Some(recursion_level) = recursion_level {
            if recursion_level >= 5 {
                info!(
                    "Recursion level exceeded, giving up on getting post from subreddit: {}",
                    subreddit
                );
                error!("Recursion level exceeded, giving up on getting post from subreddit");
                return Err(e);
            }
        }

        post = get_subreddit(
            &subreddit,
            redis,
            mongodb,
            channel,
            recursion_level.map(|x| x + 1),
        )
        .await;
    }

    post
}

/// Gets a post from a subreddit by searching for a term. Returns `None` if no post is found matching search.
#[instrument(skip(con))]
pub async fn get_subreddit_search<'a>(
    subreddit: &str,
    search: &str,
    con: &mut MultiplexedConnection,
    channel: GenericChannelId,
) -> Result<Option<PostInContext>, Error> {
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

    Ok(Some(post?))
}

#[instrument(skip(con))]
pub async fn list_contains(
    element: &str,
    list: &str,
    con: &mut MultiplexedConnection,
) -> Result<bool, Error> {
    let position: Option<u16> = con
        .lpos(list, element, redis::LposOptions::default())
        .await?;

    let to_return = match position {
        Some(_) => Ok(true),
        None => Ok(false),
    };

    to_return
}

pub async fn check_subreddit_valid(
    con: &mut MultiplexedConnection,
    web_client: &reqwest::Client,
    subreddit: &str,
) -> Result<SubredditStatus, Error> {
    debug!("Checking subreddit validity: {}", subreddit);

    let access_token = get_reddit_access_token(con, "", "", Some(web_client), None).await?;

    let res = web_client
        .head(format!("https://oauth.reddit.com/r/{}.json", subreddit))
        .header("Authorization", format!("bearer {}", access_token))
        .send()
        .await?;

    debug!("Subreddit check response: {:?}", res);
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
