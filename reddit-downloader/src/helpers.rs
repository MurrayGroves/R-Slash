use crate::helpers::Embeddability::{Embeddable, NeedsProcessing, NotEmbeddable, TextOnly};
use crate::{NewPost, PostInList};
use anyhow::{Context, Error, anyhow};
use log::error;
use mime2ext::mime2ext;
use post_subscriber::SubscriberClient;
use redis::AsyncTypedCommands;
use rslash_common::Post;
use sentry::TransactionOrSpan;
use serde_json::Value::Null;
use serde_json::{Map, Value};
use std::time::Duration;
use tarpc::context;
use tl::ParserOptions;
use tracing::{debug, trace, warn};
use truncrate::TruncateToBoundary;

/// Extract the i.redd.it URL from a media metadata item.
fn extract_i_reddit_url(metadata: &Value) -> Result<String, Error> {
    let mime = metadata.get("m").and_then(Value::as_str).ok_or(anyhow!(
        "Media item does not have a mime type: {:?}",
        metadata
    ))?;

    let id = metadata
        .get("id")
        .and_then(Value::as_str)
        .ok_or(anyhow!("Media item does not have an id"))?;

    // Reddit API returns the non-existent mime-type "image/jpg" when it should be "image/jpeg" !!!
    let ext = if mime == "image/jpg" {
        "jpg"
    } else {
        mime2ext(mime).ok_or(anyhow!("Unsupported mime type: {}", mime))?
    };

    Ok(format!("https://i.redd.it/{}.{}", id, ext))
}

/// Extract the unprocessed embed URLs from a post. Only returns more than one in the case of a gallery post, which will not need further processing.
pub fn extract_raw_embed_urls_from_post(post: Map<String, Value>) -> Result<Vec<String>, Error> {
    let is_gallery = post
        .get("is_gallery")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(if is_gallery {
        post.get("media_metadata")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                debug!("Post Object: {:?}", post);
                error!("Post is gallery but media_metadata is not an object");
                anyhow!("Post is gallery but media_metadata is not an object")
            })?
            .into_iter()
            .map(|(_, value)| extract_i_reddit_url(value))
            .collect::<Result<_, _>>()?
    } else {
        let mut url = post
            .get("url")
            .and_then(Value::as_str)
            .ok_or(anyhow!("Post does not have a URL"))?;

        if url.starts_with("https://v.redd.it") {
            url = post["media"]["reddit_video"]["dash_url"]
                .as_str()
                .ok_or(anyhow!("Post does not have a dash URL"))?;
        };

        vec![url.to_string()]
    })
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Embeddability {
    Embeddable,
    NeedsProcessing,
    NotEmbeddable, // Post contains a non-embeddable link, so we'll send it as a normal link, but also need to fetch thumbnail
    TextOnly,      // Post contains only text so is inherently embeddable
}

pub fn media_url_embeddability(url: &str) -> Embeddability {
    if (url.ends_with(".gif")
        || url.ends_with(".png")
        || url.ends_with(".jpg")
        || url.ends_with(".jpeg")
        || url.ends_with(".mp4"))
        && !url.contains("redgifs.com")
    {
        Embeddable
    } else if url.contains("imgur.com") || url.contains("redgifs.com") || url.contains(".mpd") {
        NeedsProcessing
    } else if url.starts_with("https://www.reddit.com/r/") {
        TextOnly
    } else {
        debug!("URL not embeddable and not convertable: {}", url);
        NotEmbeddable
    }
}

/// Create RediSearch index for subreddit
#[tracing::instrument(skip(con))]
pub async fn create_index(
    con: &mut redis::aio::MultiplexedConnection,
    index: String,
    prefix: String,
) -> Result<(), Error> {
    Ok(redis::cmd("FT.CREATE")
        .arg(index)
        .arg("PREFIX")
        .arg("1")
        .arg(prefix)
        .arg("SCHEMA")
        .arg("title")
        .arg("TEXT")
        .arg("score")
        .arg("NUMERIC")
        .arg("SORTABLE")
        .arg("author")
        .arg("TEXT")
        .arg("SORTABLE")
        .arg("flair")
        .arg("TAG")
        .arg("timestamp")
        .arg("NUMERIC")
        .arg("SORTABLE")
        .query_async(con)
        .await?)
}

/// Check if RediSearch index exists
#[tracing::instrument(skip(con))]
pub async fn index_exists(con: &mut redis::aio::MultiplexedConnection, index: String) -> bool {
    match redis::cmd("FT.INFO")
        .arg(index)
        .query_async::<redis::Value>(con)
        .await
    {
        Ok(_) => true,
        Err(_) => false,
    }
}

pub async fn process_post_metadata(
    parent_span: Option<TransactionOrSpan>,
    post: Result<(usize, Map<String, Value>), Error>,
    subreddit_name: &mut Option<String>,
    con: &mut redis::aio::MultiplexedConnection,
    subscriber: SubscriberClient,
    web_client: &reqwest::Client,
    existing_posts: &mut Vec<PostInList>,
    post_list: &mut Vec<PostInList>,
    subreddit: &str,
) -> Result<(), Error> {
    // Create a new transaction as an independent continuation
    let ctx = sentry::TransactionContext::continue_from_span(
        "process subreddit post",
        "process_post",
        parent_span.clone(),
    );

    let transaction = sentry::start_transaction(ctx);
    let (_, post) = match post {
        Ok(x) => x,
        Err(x) => {
            let txt = format!("Failed to get post: {}", x);
            warn!("{}", txt);
            sentry::capture_message(&txt, sentry::Level::Warning);
            return Err(x);
        }
    };

    trace!("{:?} - {:?}", post["title"], post["url"]);

    // Redis key for this post
    let key: String = format!(
        "subreddit:{}:post:{}",
        post["subreddit"]
            .to_string()
            .replace("\"", "")
            .to_lowercase(),
        &post["id"].to_string().replace('"', "")
    );

    if subreddit_name.is_none() {
        *subreddit_name = Some(post["subreddit"].to_string().replace("\"", ""));
    }

    let exists = existing_posts.iter().any(|x| match x {
        PostInList::Existing(x) => x == &key,
        _ => false,
    });

    // If the post has been removed by a moderator, remove it from the DB and skip it
    if post.get("removed_by_category").unwrap_or(&Null) != &Null
        || post["author"].to_string().replace('"', "") == "[deleted]"
    {
        debug!("Post is deleted");
        if exists {
            debug!("Removing post {:?} from DB", post["id"]);
            match con
                .lrem(
                    format!("subreddit:{}:posts", subreddit),
                    0,
                    format!(
                        "subreddit:{}:post:{}",
                        subreddit,
                        &post["id"].to_string().replace('"', "")
                    ),
                )
                .await
            {
                Ok(x) => {
                    if x == 0 {
                        warn!("Post not found in DB list to remove");
                    }
                }
                Err(x) => {
                    let txt = format!("Failed to remove post from DB: {}", x);
                    warn!("{}", txt);
                    sentry::capture_message(&txt, sentry::Level::Warning);
                }
            };
            match con
                .del(format!(
                    "subreddit:{}:post:{}",
                    subreddit,
                    &post["id"].to_string().replace('"', "")
                ))
                .await
            {
                Ok(x) => {
                    if x == 0 {
                        warn!("Post was not found in DB to remove");
                    }
                }
                Err(x) => {
                    let txt = format!("Failed to remove post from DB: {}", x);
                    warn!("{}", txt);
                }
            };

            existing_posts.retain(|x| {
                if let PostInList::Existing(x) = x {
                    x != &key
                } else {
                    false
                }
            });
        }

        // Post has been removed, so we don't need to process it further
        return Ok(());
    }

    if let Ok(media_deleted) = con
        .sismember(
            "media_deleted_posts".to_string(),
            format!("{}:{}", subreddit, post["id"]),
        )
        .await
    {
        if media_deleted {
            debug!("Post is marked as having deleted media");
            return Ok(());
        }
    } else {
        let txt = "Failed to check if post has deleted media";
        warn!("{}", txt);
    }

    if exists {
        trace!("Post already exists in DB");
        // Update post's score with new score
        match con
            .hset(&key, "score", post["score"].as_i64().unwrap_or(0))
            .await
        {
            Ok(_) => {}
            Err(x) => {
                let txt = format!("Failed to update score in redis: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
            }
        };

        post_list.push(key.into());
        return Ok(());
    }

    debug!(
        "New post not in DB: {:?} - {:?}",
        post["title"], post["url"]
    );

    let urls = extract_raw_embed_urls_from_post(post.clone())?;

    let embeddability =
        media_url_embeddability(urls.iter().next().ok_or(anyhow!("Post has no embed URL"))?);

    let linked_url = if let Embeddability::NotEmbeddable = embeddability {
        urls.first().cloned()
    } else {
        None
    };

    let embed_urls = match embeddability {
        TextOnly => Vec::new(),
        NotEmbeddable => Vec::new(),
        _ => urls,
    };

    let mut linked_url_image = None;
    let mut linked_url_title = None;
    let mut linked_url_description = None;

    if let Some(ref linked_url) = linked_url {
        let html = web_client
            .get(linked_url)
            .timeout(Duration::from_secs(10))
            .send()
            .await?
            .text()
            .await?;
        let dom = tl::parse(&html, ParserOptions::default().track_classes())?;
        let metas = dom.query_selector("meta");
        if let Some(metas) = metas {
            for node_handle in metas {
                debug!("Node Handle: {:?}", node_handle);
                if let Some(node) = node_handle.get(dom.parser())
                    && let Some(tag) = node.as_tag()
                    && let Some(Some(property)) = tag.attributes().get("property")
                    && let Some(Some(content)) = tag.attributes().get("content")
                {
                    let property = property.as_utf8_str();
                    let content = content.as_utf8_str();
                    debug!("Property: {:?}, Content: {:?}", property, content);
                    match property.as_str() {
                        "og:title" => linked_url_title = Some(content.to_string()),
                        "og:image" => linked_url_image = Some(content.to_string()),
                        "og:description" => linked_url_description = Some(content.to_string()),
                        _ => (),
                    }
                }
            }
        }
    }

    let timestamp = post["created_utc"].as_f64().unwrap() as u64;
    let mut title = post["title"]
        .as_str()
        .context("Title wasn't string?")?
        .replace("&amp;", "&");
    // Truncate title length to 256 chars (Discord embed title limit)
    title = (*title.as_str()).truncate_to_boundary(256).to_string();

    let post_object = NewPost {
        redis_key: key.clone(),
        post: Post {
            score: post["score"].as_i64().unwrap_or(0) as isize,
            url: format!(
                "https://reddit.com{}",
                post["permalink"].to_string().replace('"', "")
            ),
            title,
            embed_urls,
            author: post["author"].to_string().replace('"', ""),
            id: post["id"].to_string().replace('"', ""),
            timestamp,
            text: post["selftext"]
                .as_str()
                .map(|x| x.to_string())
                .filter(|x| !x.is_empty()),

            linked_url,
            linked_url_description,
            linked_url_title,
            linked_url_image,
        },
        embeddability,
    };

    if let Embeddability::NeedsProcessing = post_object.embeddability {
        post_list.push(post_object.into());
    } else {
        // If it doesn't need further processing, we can add it right now
        debug!(
            "Adding to redis immediately with key {:?}: {:?}",
            key, post_object
        );

        // Push post to Redis
        let value = Vec::from(&post_object.post);
        match con.hset_multiple(&key, &value).await {
            Ok(_) => {}
            Err(x) => {
                let txt = format!("Failed to set post in redis: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
            }
        };

        // Notify subscriber service that a new post has been detected
        match subscriber
            .notify(
                context::current(),
                post["subreddit"].to_string().replace("\"", ""),
                post["id"].to_string().replace('"', ""),
            )
            .await
        {
            Ok(_) => {}
            Err(x) => {
                let txt = format!("Failed to notify subscriber: {}", x);
                warn!("{}", txt);
                sentry::capture_message(&txt, sentry::Level::Warning);
            }
        };

        post_list.push(post_object.into());

        // Get new list of post keys (existing posts + new posts that don't need processing)
        let mut post_keys: Vec<&String> = post_list
            .iter()
            .filter(|p| match p {
                PostInList::New(p) => p.embeddability != Embeddability::NeedsProcessing,
                PostInList::Existing(_) => true,
            })
            .map(|p| match p {
                PostInList::New(p) => &p.redis_key,
                PostInList::Existing(key) => &key,
            })
            .collect();

        // Append post_list with existing posts that aren't in the current fetch
        let existing_posts: Vec<String> = existing_posts
            .iter()
            .map(|x| match x {
                PostInList::Existing(x) => x.clone(),
                PostInList::New(x) => x.redis_key.clone(),
            })
            .filter(|x| !post_keys.contains(&x))
            .collect();
        post_keys = post_keys.into_iter().chain(existing_posts.iter()).collect();

        // Update redis with new post keys
        redis::pipe()
            .atomic()
            .del::<String>(format!("subreddit:{}:posts", subreddit))
            .rpush::<String, Vec<&String>>(format!("subreddit:{}:posts", subreddit), post_keys)
            .query_async::<()>(con)
            .await?;
    }

    transaction.finish();
    Ok(())
}
