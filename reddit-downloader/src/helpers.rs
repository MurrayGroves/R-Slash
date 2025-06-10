use crate::helpers::Embeddability::{Embeddable, NeedsProcessing, NotEmbeddable};
use anyhow::{Error, anyhow};
use log::error;
use mime2ext::mime2ext;
use serde_json::{Map, Value};
use tracing::debug;

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
                error!(
                    "Post is gallery but media_metadata is not an object: {:?}",
                    post
                );
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

pub enum Embeddability {
    Embeddable,
    NeedsProcessing,
    NotEmbeddable,
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
