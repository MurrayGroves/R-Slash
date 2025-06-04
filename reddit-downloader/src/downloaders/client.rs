use super::dash;
use super::generic;
use super::imgur;
use super::redgifs;
use crate::Post;
use anyhow::{Context, Error, Result, anyhow, bail};
use base64::Engine;
use post_subscriber::SubscriberClient;
use redis::AsyncCommands;
use reqwest::StatusCode;
use sentry::integrations::anyhow::capture_anyhow;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tarpc::context;
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::info;
use tracing::warn;
use tracing::{debug, error, trace};

pub struct Token {
    url: String,
    token: String,
    expiry: i64,
}

impl Token {
    async fn update_token(url: &str) -> Result<(String, i64)> {
        let mut req = reqwest::Client::builder()
            .user_agent("Booty Bot")
            .build()?
            .get(url)
            .send()
            .await?;
        while req.status() == 429 {
            debug!("Token request status: {}", req.status());
            debug!("Token request headers: {:?}", req.headers());
            info!("Waiting 10 minutes for token rate limit");
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            req = reqwest::Client::new().get(url).send().await?;
        }

        let mut text = req.text().await?;
        debug!("Token request body: {}", text);
        // Redgifs returns tokens like this sometimes???????
        let token = if text.contains("html") {
            text = text.replace("<html><head><title>Loading...</title></head><body><script type='text/javascript'>window.location.replace('https://api.regifs.com/v2/auth/temporary?ch=1&js=", "");
            text = text
                .split("&sid")
                .next()
                .ok_or(Error::msg("Failed to parse token"))?
                .to_string();
            text
        } else {
            let json: Value = serde_json::from_str(&text)?;
            json["token"]
                .as_str()
                .ok_or(Error::msg("Failed to parse token"))?
                .to_string()
        };
        debug!("Token: {}", token);
        let token_data = token
            .split(".")
            .nth(1)
            .ok_or(Error::msg("Failed to parse token"))?;
        let decoded = base64::engine::general_purpose::STANDARD_NO_PAD.decode(token_data)?;
        let decoded = String::from_utf8(decoded)?;
        let json: Value = serde_json::from_str(&decoded)?;
        let expiry = json["exp"]
            .as_i64()
            .ok_or(Error::msg("Failed to parse expiry"))?;

        return Ok((token.to_string(), expiry));
    }

    pub async fn new(url: String) -> Result<Self> {
        let (token, expiry) = Self::update_token(&url).await?;

        Ok(Self { url, token, expiry })
    }

    #[tracing::instrument(skip(self))]
    pub async fn get_token(&mut self) -> Result<&str> {
        if self.expiry < chrono::Utc::now().timestamp() - 60 {
            let (token, expiry) = Self::update_token(&self.url).await?;
            self.token = token;
            self.expiry = expiry;
        }

        Ok(&self.token)
    }
}

struct LimitState {
    remaining: u64,
    reset: i64, // Unix timestamp of when the limit resets
}

// Struct for tracking rate limits
#[derive(Clone)]
pub struct Limiter {
    state: Arc<tokio::sync::Mutex<LimitState>>,
    per_minute: Option<u64>, // User can specify rate limit per minute, otherwise use the headers
    name: String,
}

impl Limiter {
    pub fn new(per_minute: Option<u64>, name: String) -> Self {
        Self {
            state: Arc::new(tokio::sync::Mutex::new(LimitState {
                remaining: 1,
                reset: 0,
            })),
            name,
            per_minute,
        }
    }

    // Update the rate limit based on the headers from the last request
    pub async fn update_headers(
        &self,
        headers: &reqwest::header::HeaderMap,
        status: StatusCode,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().await;

        debug!(
            "Updating rate limit headers for {}: {:?}",
            self.name, headers
        );

        // If this limiter is using a per minute limit
        if let Some(per_minute) = self.per_minute {
            if state.remaining != 0 {
                state.remaining -= 1
            };
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }

            if let Some(remaining) = headers.get("Retry-After") {
                state.reset =
                    chrono::Utc::now().timestamp() + remaining.to_str()?.parse::<i64>()?;
                state.remaining = 0;
            }
        } else {
            if let Some(remaining) = headers.get("X-RateLimit-UserRemaining") {
                state.remaining = remaining.to_str()?.parse()?;
            } else if let Some(reset) = headers.get("X-RateLimit-UserReset") {
                state.reset = reset.to_str()?.parse()?;
            } else {
                if status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::FORBIDDEN
                    || status == StatusCode::UNAUTHORIZED
                {
                    // Wait 10 minutes if we hit the rate limit, and they didn't tell us when to try again
                    warn!(
                        "We hit the rate limit for {} with status {} waiting 10 mins as a pre-caution with headers: {:?}",
                        self.name, status, headers
                    );
                    state.reset = chrono::Utc::now().timestamp() + 600;
                    state.remaining = 0;
                }

                if let Some(remaining) = headers.get("x-ratelimit-remaining") {
                    state.remaining = remaining
                        .to_str()?
                        .split(".")
                        .nth(0)
                        .ok_or(Error::msg("Failed to parse rate limit"))?
                        .parse()?;
                }
                if let Some(reset) = headers.get("x-ratelimit-reset") {
                    state.reset =
                        chrono::Utc::now().timestamp() + reset.to_str()?.parse::<i64>()?;
                }
            }
        }

        Ok(())
    }

    pub async fn update(&self) {
        let mut state = self.state.lock().await;
        if !state.remaining == 0 {
            state.remaining -= 1;
        }
        if let Some(per_minute) = self.per_minute {
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }
        }
    }

    // Wait until we are allowed to request again
    #[tracing::instrument(skip(self))]
    pub async fn wait(&self) {
        let state = self.state.lock().await;
        if state.remaining < 3 && state.reset > chrono::Utc::now().timestamp() {
            let wait = state.reset - chrono::Utc::now().timestamp();
            drop(state);
            info!("Waiting {} seconds for rate limit on {}", wait, self.name);
            tokio::time::sleep(std::time::Duration::from_secs(wait as u64)).await;
        }
    }
}

/// Client for downloading mp4s from various sources and converting them to gifs for embedding
#[derive(Clone)]
pub struct Client {
    imgur: Option<imgur::Client>,
    generic: generic::Client,
    dash: Option<dash::Client>,
    posthog: Option<posthog::Client>,
    redgifs: Option<redgifs::Client>,
    path: String,
    process_lock: Arc<Mutex<()>>,
    requests: Arc<Mutex<VecDeque<(String, Vec<Post>)>>>,
    redis: redis::aio::MultiplexedConnection,
    subscriber_client: SubscriberClient,
    /// The time the current subreddit started processing, and which subreddit that is.
    subreddit_started_at: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Client {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    /// * `imgur_client_id` - Client ID for the Imgur API, if missing then Imgur downloads will fail
    /// * `posthog_client` - Optional Posthog client for tracking downloads and api usage
    pub async fn new(
        path: String,
        imgur_client_id: Option<String>,
        posthog_client: Option<posthog::Client>,
        redis: redis::aio::MultiplexedConnection,
        subscriber_client: SubscriberClient,
    ) -> Result<Self> {
        let new = Self {
            imgur: match imgur_client_id {
                Some(client_id) => Some(imgur::Client::new(
                    path.clone(),
                    Arc::new(client_id),
                    Limiter::new(None, "imgur".to_string()),
                )),
                None => None,
            },
            generic: generic::Client::new(
                path.clone(),
                Limiter::new(Some(60), "generic".to_string()),
            ),
            posthog: posthog_client,
            dash: Some(dash::Client::new(
                path.clone(),
                Limiter::new(Some(60), "dash".to_string()),
            )),
            redgifs: Some(
                redgifs::Client::new(path.clone(), Limiter::new(Some(60), "redgifs".to_string()))
                    .await?,
            ),
            path,
            process_lock: Arc::new(Mutex::new(())),
            requests: Arc::new(Mutex::new(VecDeque::new())),
            redis,
            subscriber_client,
            subreddit_started_at: Arc::new(Mutex::new(None)),
        };

        let new_clone = new.clone();
        tokio::spawn(async move {
            loop {
                match new_clone.clone().process_queue().await {
                    Ok(_) => {
                        error!("Queue processing terminated");
                    }
                    Err(e) => {
                        error!("Failed to process queue: {}", e);
                        sentry::capture_message(
                            &format!("Failed to process queue: {}", e),
                            sentry::Level::Warning,
                        );
                    }
                }
            }
        });
        Ok(new)
    }

    #[tracing::instrument(skip(self, posts))]
    async fn process_subreddit_posts(
        &mut self,
        subreddit: String,
        mut posts: Vec<Post>,
    ) -> Result<()> {
        debug!("Processing media for subreddit: {}", subreddit);

        let mut started_at = self.subreddit_started_at.lock().await;
        *started_at = Some((subreddit.clone(), tokio::time::Instant::now()));
        drop(started_at);

        for i in 0..posts.len() {
            // If post has already been processed, or is existing post, skip
            let post = if let Some(Post::New(post)) = posts.get_mut(i) {
                if post.needs_processing {
                    post
                } else {
                    continue;
                }
            } else {
                continue;
            };

            debug!("Processing media for post: {}", post.id);
            post.embed_url = match self.resolve_embed_url(&post.embed_url, &post.id).await {
                Ok(url) => url,
                Err(e) => {
                    warn!("Failed to process media: {:?}", e);
                    continue;
                }
            };

            if !post.embed_url.starts_with("http") {
                post.embed_url = format!("https://cdn.rsla.sh/gifs/{}", post.embed_url);
            };

            post.needs_processing = false;

            let redis_key = post.redis_key.clone();
            let post_id = post.id.clone();

            let post = Vec::from(post.clone());

            let post_keys: Vec<&String> = posts
                .iter()
                .filter(|p| match p {
                    Post::New(p) => !p.needs_processing,
                    Post::Existing(_) => true,
                })
                .map(|p| match p {
                    Post::New(p) => &p.redis_key,
                    Post::Existing(key) => &key,
                })
                .collect();

            // Push post to Redis
            match self
                .redis
                .hset_multiple::<&str, String, String, redis::Value>(&redis_key, &post)
                .await
                .context("Setting post details in Redis")
            {
                Ok(_) => {}
                Err(x) => {
                    warn!("{:?}", x);
                    capture_anyhow(&x);
                }
            };

            // Notify subscriber service that a new post has been detected
            match self
                .subscriber_client
                .notify(context::current(), subreddit.to_string(), post_id)
                .await
                .context("Notifying subscriber")
            {
                Ok(_) => {}
                Err(x) => {
                    warn!("{:?}", x);
                    capture_anyhow(&x);
                }
            };

            // Update redis with new post keys
            redis::pipe()
                .atomic()
                .del::<String>(format!("subreddit:{}:posts", subreddit))
                .rpush::<String, Vec<&String>>(format!("subreddit:{}:posts", subreddit), post_keys)
                .query_async::<()>(&mut self.redis)
                .await?;
        }

        let mut started_at = self.subreddit_started_at.lock().await;
        *started_at = None;
        drop(started_at);

        debug!("Finished media for subreddit: {}", subreddit);

        Ok(())
    }

    async fn process_queue(mut self) -> Result<()> {
        loop {
            let (subreddit, posts) = {
                let mut interval = tokio::time::interval(Duration::from_millis(100));
                loop {
                    interval.tick().await;
                    if !self.requests.lock().await.is_empty() {
                        break;
                    }
                }
                let mut requests = self.requests.lock().await;
                requests
                    .pop_front()
                    .ok_or(anyhow!("Failed to pop request"))?
            };

            self.process_subreddit_posts(subreddit.clone(), posts)
                .await?;
        }
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    #[tracing::instrument(skip(self))]
    async fn mp4_to_gif(&self, path: &str) -> Result<String, Error> {
        let _lock = self.process_lock.lock().await;
        let parent_span = sentry::configure_scope(|scope| scope.get_span());
        let span: sentry::TransactionOrSpan = match &parent_span {
            Some(parent) => parent.start_child("subtask", "description").into(),
            None => {
                let ctx = sentry::TransactionContext::new("task", "op");
                sentry::start_transaction(ctx).into()
            }
        };

        let new_path: String = format!("{}.gif", path.replace(".mp4", ""));
        let full_path = format!("{}/{}", self.path, path);
        let new_full_path = format!("{}/{}", self.path, new_path);

        let scale = ",scale=-1:'min(360,ih)':flags=lanczos";

        {
            let span = span.start_child("ffmpeg", "converting mp4 to gif");

            let output = Command::new("ffmpeg")
                .arg("-y")
                .arg("-i")
                .arg(&full_path)
                .arg("-vf")
                .arg(format!(
                    "fps=fps=15{},split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse",
                    scale
                ))
                .arg("-loop")
                .arg("0")
                .arg(&new_full_path)
                .output()?;

            if !output.status.success() {
                std::io::stderr().write_all(&output.stderr)?;
                warn!(
                    "Failed to convert mp4 to gif: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Err(Error::msg(format!(
                    "ffmpeg failed: {}\n{}",
                    output.status.to_string(),
                    output.status
                )))
                .with_context(|| format!("path: {}", full_path))
                .with_context(|| format!("STDERR: {}", String::from_utf8_lossy(&output.stderr)))?;
            }

            span.finish();
        }

        let size = std::fs::metadata(&new_full_path)?.len();
        let output = Command::new("cp")
            .arg(&new_full_path)
            .arg("temp.gif")
            .output()?;

        if !output.status.success() {
            warn!(
                "Failed to optimise gif at : {}, with error: {}",
                full_path,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let span = span.start_child("gifsicle", "optimising gif");
        let output = Command::new("gifsicle")
            .arg("-O3")
            .arg("--lossy=80")
            .arg("--colors=128")
            .arg("--batch")
            .arg("temp.gif")
            .output()?;

        if !output.status.success() {
            warn!(
                "Failed to optimise gif at : {}, with error: {}",
                full_path,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        drop(span);

        if size < std::fs::metadata("temp.gif")?.len() {
            debug!(
                "Size increased from {} to {}",
                size,
                std::fs::metadata("temp.gif")?.len()
            );
        } else {
            debug!(
                "Old size: {}, new size: {}",
                size,
                std::fs::metadata("temp.gif")?.len()
            );

            let output = Command::new("mv")
                .arg("temp.gif")
                .arg(&new_full_path)
                .output()?;

            if !output.status.success() {
                warn!(
                    "Failed to moved optimised gif at : {}, with error: {}",
                    full_path,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        let output = Command::new("rm").arg(&full_path).output()?;

        if !output.status.success() {
            warn!(
                "Failed to delete mp4 at : {}, with error: {}",
                full_path,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(new_path)
    }

    /// Download a single mp4 from a url, and return the path to the gif
    #[tracing::instrument(skip(self))]
    async fn resolve_embed_url(&self, url: &str, filename: &str) -> Result<String, Error> {
        debug!("Requesting: {} for filename {}", url, filename);
        let mut path = if url.contains("redgifs.com") {
            match &self.redgifs {
                Some(redgifs) => redgifs
                    .request(url, filename)
                    .await
                    .context("Fetching from redgifs")?,
                None => Err(Error::msg("Redgifs client not set"))?,
            }
        } else if url.contains("imgur.com") {
            match &self.imgur {
                Some(imgur) => imgur
                    .request(url, filename)
                    .await
                    .context("Requesting from imgur")?,
                None => Err(Error::msg("Imgur client ID not set"))?,
            }
        } else if url.contains(".mpd") {
            match &self.dash {
                Some(dash) => dash
                    .request(url, filename)
                    .await
                    .context("Requesting from dash")?,
                None => Err(Error::msg("Dash client not set"))?,
            }
        } else {
            self.generic
                .request(url, filename)
                .await
                .context("Using generic downloader")?
        };

        debug!("Path returned: {}", path);

        if path.ends_with(".mp4") {
            // Get size of the mp4
            let size = std::fs::metadata(format!("{}/{}", self.path, path))?.len();

            if size / 1024 == 0 {
                bail!("File is empty")
            }

            // If the mp4 is small enough, convert it to a gif so it will autoplay
            if size < 2 * 1024 * 1024 {
                debug!(
                    "File is small enough to convert to GIF ({} KB)",
                    size / 1024
                );

                // Check if file contains audio streams
                let output = Command::new("ffprobe")
                    .arg("-i")
                    .arg(format!("{}/{}", self.path, path))
                    .arg("-show_streams")
                    .arg("-select_streams")
                    .arg("a")
                    .arg("-loglevel")
                    .arg("error")
                    .output()?;

                let process_to_gif = if output.stdout.len() > 10 {
                    debug!("File contains audio streams, checking average volume");
                    let output = Command::new("ffmpeg")
                        .arg("-i")
                        .arg(format!("{}/{}", self.path, path))
                        .arg("-filter:a")
                        .arg("astats")
                        .arg("-f")
                        .arg("null")
                        .arg("/dev/null")
                        .output()?;

                    // Find the average volume line
                    let output_combined = format!(
                        "{}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let volume = output_combined
                        .lines()
                        .find(|line| line.contains("RMS level dB"))
                        .ok_or(Error::msg(format!(
                            "Failed to find mean volume, ffmpeg output:\n {}",
                            output_combined
                        )))?
                        .to_string();

                    // Extract the volume from the line
                    let volume = volume
                        .split(":")
                        .nth(1)
                        .ok_or(Error::msg("Failed to extract volume"))?;

                    let silent = volume.contains("inf")
                        || volume.trim().parse::<f32>().map_err(|err| {
                            Error::msg(format!(
                                "Failed to parse mean volume from string: {}",
                                volume
                            ))
                        })? < -60.0;

                    silent
                } else {
                    false
                };

                if process_to_gif {
                    path = self.mp4_to_gif(&path).await.context("Processing gif")?
                };
            } else {
                debug!("File is too large to convert to GIF ({} KB)", size / 1024);
            }
        }

        Ok(path)
    }

    /// Check if previous requests for subreddit have finished
    pub async fn is_finished(&self, subreddit: &str) -> bool {
        let subreddit = subreddit.to_lowercase();
        if let Some(started_at) = self.subreddit_started_at.lock().await.as_ref() {
            if started_at.1.elapsed() > Duration::from_secs(600) {
                error!(
                    "Some subreddit has been processing for over 10 minutes, current requests queue is {:?}",
                    self.requests.lock().await
                );
            }
        }

        let requests = self.requests.lock().await;
        if requests.iter().any(|request| request.0 == subreddit) {
            debug!("Returning false, subreddit is already in queue");
            return false;
        }
        drop(requests);

        if let Some(started_at) = self.subreddit_started_at.lock().await.as_ref() {
            if started_at.0 == subreddit {
                debug!("Returning false, subreddit is currently being processed");
                return false;
            }
        }

        debug!("Returning true, subreddit is not in queue or being processed");
        true
    }

    /// Add subreddit to process queue
    pub async fn queue_subreddit_for_processing(
        &self,
        subreddit: &str,
        posts: Vec<Post>,
    ) -> Result<()> {
        let subreddit = subreddit.to_lowercase();
        debug!(
            "Adding subreddit to process queue: {}, {:?}",
            subreddit, posts
        );

        let mut requests = self.requests.lock().await;
        debug!(
            "Requests queue: {:?}",
            requests
                .iter()
                .map(|(sub, _)| sub)
                .collect::<Vec<&String>>()
        );
        requests.push_back((subreddit.to_string(), posts));
        Ok(())
    }
}
