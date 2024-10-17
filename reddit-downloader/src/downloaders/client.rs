use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use anyhow::bail;
use anyhow::{anyhow, Context, Error, Result};
use base64::Engine;
use reqwest::StatusCode;
use sentry::Scope;
use serde_json::json;
use serde_json::Value;
use tracing::debug;
use tracing::info;
use tracing::warn;

use super::dash;
use super::generic;
use super::imgur;
use super::redgifs;

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
}

impl Limiter {
    pub fn new(per_minute: Option<u64>) -> Self {
        Self {
            state: Arc::new(tokio::sync::Mutex::new(LimitState {
                remaining: 1,
                reset: 0,
            })),
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

        debug!("Updating rate limit headers: {:?}", headers);

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
                if status == StatusCode::TOO_MANY_REQUESTS {
                    // Wait 10 minutes if we hit the rate limit and they didn't tell us when to try again
                    state.reset = chrono::Utc::now().timestamp() + 600;
                    state.remaining = 0;
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
        if state.remaining == 0 && state.reset > chrono::Utc::now().timestamp() {
            let wait = state.reset - chrono::Utc::now().timestamp();
            drop(state);
            info!("Waiting {} seconds for rate limit", wait);
            tokio::time::sleep(std::time::Duration::from_secs(wait as u64)).await;
        }
    }
}

/// Client for downloading mp4s from various sources and converting them to gifs for embedding
#[derive(Clone)]
pub struct Client<'a> {
    imgur: Option<imgur::Client<'a>>,
    generic: generic::Client<'a>,
    dash: Option<dash::Client<'a>>,
    posthog: Option<posthog::Client>,
    redgifs: Option<redgifs::Client<'a>>,
    path: &'a str,
    process_lock: Arc<tokio::sync::Mutex<()>>,
}

impl<'a> Client<'a> {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    /// * `imgur_client_id` - Client ID for the Imgur API, if missing then Imgur downloads will fail
    /// * `posthog_client` - Optional Posthog client for tracking downloads and api usage
    pub async fn new(
        path: &'a str,
        imgur_client_id: Option<String>,
        posthog_client: Option<posthog::Client>,
    ) -> Result<Self> {
        Ok(Self {
            imgur: match imgur_client_id {
                Some(client_id) => Some(imgur::Client::new(
                    &path,
                    Arc::new(client_id),
                    Limiter::new(None),
                )),
                None => None,
            },
            generic: generic::Client::new(&path, Limiter::new(Some(60))),
            path: &path,
            posthog: posthog_client,
            dash: Some(dash::Client::new(&path, Limiter::new(Some(60)))),
            redgifs: Some(redgifs::Client::new(&path, Limiter::new(Some(60))).await?),
            process_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    #[tracing::instrument(skip(self))]
    async fn process(&self, path: &str) -> Result<String, Error> {
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
                std::io::stderr().write_all(&output.stderr).unwrap();
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
    pub async fn request(&self, url: &str, filename: &str) -> Result<String, Error> {
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
                Some(imgur) => imgur.request(url).await.context("Requesting from imgur")?,
                None => Err(Error::msg("Imgur client ID not set"))?,
            }
        } else if url.contains(".mpd") {
            match &self.dash {
                Some(dash) => dash.request(url).await.context("Requesting from dash")?,
                None => Err(Error::msg("Dash client not set"))?,
            }
        } else {
            self.generic.request(url).await?
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
                path = self.process(&path).await.context("Processing gif")?;
            } else {
                debug!("File is too large to convert to GIF ({} KB)", size / 1024);
            }
        }

        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Context;

    use super::*;

    #[tokio::test]
    async fn process_output_success() -> Result<(), Error> {
        let client = Client::new("test-data".into(), None, None).await?;
        client.process("test.mp4").await?;
        Ok(())
    }

    #[tokio::test]
    async fn process_filename_correct() -> Result<(), Error> {
        let client = Client::new("test-data".into(), None, None).await?;
        let path = client.process("test.mp4").await?;
        assert_eq!(path, "test.gif");
        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None).await?;

        let path = client
            .request("https://imgur.com/H48hDPg", "")
            .await
            .context("initiating request")?;
        println!("{}", path);
        assert_eq!(path, "https://i.imgur.com/H48hDPg.gif");

        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None).await?;

        let path = client
            .request("https://imgur.com/a/ErqxbAa", "")
            .await
            .context("initiating request")?;
        assert_eq!(path, "https://i.imgur.com/su3kGzS.gif");

        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None).await?;

        let path = client
            .request("https://imgur.com/gallery/rvGOIHS", "")
            .await
            .context("initiating request")?;
        println!("{}", path);
        assert!(path.ends_with(".gif"));

        let path = format!("test-data/{}", path);

        let file = tokio::fs::read(&path)
            .await
            .with_context(|| format!("Opening final gif path: {}", path))?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn redgifs() -> Result<(), Error> {
        let client = Client::new("test-data".into(), None, None).await?;

        let path = client
            .request(
                "https://www.redgifs.com/watch/elderlysupportivepeafowl",
                "test",
            )
            .await
            .context("initiating request")?;
        println!("{}", path);
        assert!(path.ends_with(".gif"));

        let path = "test-data/elderlysupportivepeafowl.gif";

        let file = tokio::fs::read(&path)
            .await
            .with_context(|| format!("Opening final gif path: {}", path))?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn parallel_request() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None).await?;

        let urls = vec![
            "https://imgur.com/gallery/rvGOIHS",
            "https://imgur.com/a/ErqxbAa",
            "https://imgur.com/H48hDPg",
        ];

        let mut tasks = HashMap::new();

        let client_arc = Arc::new(client);

        println!("Creating tasks...");

        for url in urls {
            let client = client_arc.clone();
            let task = tokio::spawn(async move {
                println!("started");
                let res = client.request(url, "").await;
                println!("done");
                return res;
            });
            tasks.insert(url.to_string(), task);
        }

        println!("Tasks: {:?}", tasks);

        let mut resolved: HashMap<String, String> = HashMap::new();

        loop {
            if tasks.len() == 0 {
                break;
            }

            let mut finished = Vec::new();
            for (url, task) in &tasks {
                if task.is_finished() {
                    println!("Task finished: {}", url);
                    finished.push(url.clone());
                }
            }

            for url in finished {
                let res = tasks
                    .remove(&url)
                    .ok_or(anyhow!("Task not found"))?
                    .await??;
                resolved.insert(url, res);
            }

            // Give tokio a chance to run the tasks
            tokio::task::yield_now().await;
        }

        assert_eq!(resolved.len(), 3);
        let first_resp = resolved
            .get("https://imgur.com/gallery/rvGOIHS")
            .ok_or(anyhow!("No response"))?
            .to_string();
        assert_eq!(first_resp, "rvGOIHS.gif".to_string());

        let first_resp = resolved
            .get("https://imgur.com/a/ErqxbAa")
            .ok_or(anyhow!("No response"))?
            .to_string();
        assert_eq!(first_resp, "https://i.imgur.com/su3kGzS.gif".to_string());

        let first_resp = resolved
            .get("https://imgur.com/H48hDPg")
            .ok_or(anyhow!("No response"))?
            .to_string();
        assert_eq!(first_resp, "https://i.imgur.com/H48hDPg.gif".to_string());

        Ok(())
    }
}
