use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Error, anyhow};
use tracing::info;
use tracing::warn;
use tracing::debug;

use super::dash;
use super::imgur;
use super::generic;
use super::redgifs;


struct LimitState {
    remaining: u64,
    reset: i64, // Unix timestamp of when the limit resets
}

// Struct for tracking rate limits
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
    pub async fn update_headers(&self, headers: &reqwest::header::HeaderMap) -> Result<(), Error>{
        let mut state = self.state.lock().await;
        if let Some(per_minute) = self.per_minute {
            state.remaining -= 1;
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }

            if let Some(remaining) = headers.get("Retry-After") {
                state.reset = chrono::Utc::now().timestamp() + remaining.to_str()?.parse::<i64>()?;
                state.remaining = 0;
            }
        } else {
            if let Some(remaining) = headers.get("X-RateLimit-UserRemaining") {
                state.remaining = remaining.to_str()?.parse()?;
            }
            if let Some(reset) = headers.get("X-RateLimit-UserReset") {
                state.reset = reset.to_str()?.parse()?;
            }
        }

        Ok(())
    }

    pub async fn update(&self) {
        let mut state = self.state.lock().await;
        state.remaining -= 1;
        if let Some(per_minute) = self.per_minute {
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }
        }
    }

    // Wait until we are allowed to request again
    pub async fn wait(&self) {
        let state = self.state.lock().await;
        if state.remaining == 0 {
            let wait = state.reset - chrono::Utc::now().timestamp();
            drop(state);
            info!("Waiting {} seconds for rate limit", wait);
            tokio::time::sleep(std::time::Duration::from_secs(wait as u64)).await;
        }
    }
}

/// Client for downloading mp4s from various sources and converting them to gifs for embedding
pub struct Client<'a> {
    imgur: Option<imgur::Client<'a>>,
    generic: generic::Client<'a>,
    dash: Option<dash::Client<'a>>,
    posthog: Option<posthog::Client>,
    redgifs: Option<redgifs::Client<'a>>,
    path: &'a str,
}

impl <'a>Client<'a> {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    /// * `imgur_client_id` - Client ID for the Imgur API, if missing then Imgur downloads will fail
    /// * `posthog_client` - Optional Posthog client for tracking downloads and api usage
    pub fn new(path: &'a str, imgur_client_id: Option<String>, posthog_client: Option<posthog::Client>) -> Self {
        Self {
            imgur: match imgur_client_id {
                Some(client_id) => Some(imgur::Client::new(&path, Arc::new(client_id), Limiter::new(None))),
                None => None,
            },
            generic: generic::Client::new(&path, Limiter::new(Some(60))),
            path: &path,
            posthog: posthog_client,
            dash: Some(dash::Client::new(&path, Limiter::new(Some(60)))),
            redgifs: Some(redgifs::Client::new(&path, Limiter::new(Some(60)))),
        }
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    async fn process(&self, path: &str) -> Result<String, Error> {
        let new_path: String = format!("{}.gif", path.replace(".mp4", ""));
        let full_path = format!("{}/{}", self.path, path);
        let new_full_path = format!("{}/{}", self.path, new_path);

        /*let output = Command::new("mediainfo")
            .arg("--output=JSON")
            .arg(&full_path)
            .output()?;

        let output = String::from_utf8_lossy(&output.stdout);
        let output: serde_json::Value = serde_json::from_str(&output).context("Parsing mediainfo output")?;
        // First track is used for general metadata, second track contains video metadata
        let width = output["media"]["track"][1]["Width"].as_str().ok_or(Error::msg("Failed to parse mediainfo output"))?.parse::<u32>()?;
        let height = output["media"]["track"][1]["Height"].as_str().ok_or(Error::msg("Failed to parse mediainfo output"))?.parse::<u32>()?;

        // If the video is too big, scale it down
        let scale = if width > height {
            format!(",scale=320:-1:flags=lanczos")
        } else {
            format!(",scale=-1:300:flags=lanczos")
        };

        // If the video is small enough, don't scale it
        let scale = if width < 320 && height < 300 {
            ""
        } else {
            &scale
        };*/

        let scale = ",scale=-1:'min(360,ih)':flags=lanczos";

        let output = Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(&full_path)
            .arg("-vf")
            .arg(format!("fps=fps=15{},split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse", scale))
            .arg("-loop")
            .arg("0")
            .arg(&new_full_path)
            .output()?;

        if !output.status.success() {
            std::io::stderr().write_all(&output.stderr).unwrap();
            warn!("Failed to convert mp4 to gif: {}", String::from_utf8_lossy(&output.stderr));
            Err(Error::msg(format!("ffmpeg failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", full_path))?;
        }   


        let size = std::fs::metadata(&new_full_path)?.len();
        let mut count = 0;
        while count < 5 && size > 8 * 1024 * 1024 {
            let output = Command::new("mv")
            .arg(&new_full_path)
            .arg("temp.gif")
            .output()?;
    
            if !output.status.success() {
                warn!("Failed to optimise gif at : {}, with error: {}", full_path, String::from_utf8_lossy(&output.stderr));
            }
    
            let output = Command::new("gifsicle")
                .arg("-O3")
                .arg("--lossy=80")
                .arg("--colors=128")
                .arg("--batch")
                .arg("temp.gif")
                .output()?;

            if !output.status.success() {
                warn!("Failed to optimise gif at : {}, with error: {}", full_path, String::from_utf8_lossy(&output.stderr));
            }
            
            if size < std::fs::metadata("temp.gif")?.len() {
                debug!("Size increased, breaking");
                break;
            }
    
            let output = Command::new("mv")
                .arg("temp.gif")
                .arg(&new_full_path)
                .output()?;

            if !output.status.success() {
                warn!("Failed to optimise gif at : {}, with error: {}", full_path, String::from_utf8_lossy(&output.stderr));
            }

            count += 1;
        }

        let output = Command::new("rm")
            .arg(&full_path)
            .output()?;

        if !output.status.success() {
            warn!("Failed to delete mp4 at : {}, with error: {}", full_path, String::from_utf8_lossy(&output.stderr));
        }

        Ok(new_path)
    }

    /// Download a single mp4 from a url, and return the path to the gif
    pub async fn request(&self, url: &str) -> Result<String, Error> {
        let mut path = if url.ends_with(".mp4") {
            self.generic.request(url).await?
        } else if url.contains("redgifs.com") {
            match &self.redgifs {
                Some(redgifs) => redgifs.request(url).await.context("Requesting from redgifs")?,
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

        if path.ends_with(".mp4") {
            path = self.process(&path).await.context("Processing gif")?;
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
        let client = Client::new("test-data".into(), None, None);
        client.process("test.mp4").await?;
        Ok(())
    }

    #[tokio::test]
    async fn process_filename_correct() -> Result<(), Error> {
        let client = Client::new("test-data".into(), None, None);
        let path = client.process("test.mp4").await?;
        assert_eq!(path, "test.gif");
        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

        let path = client.request("https://imgur.com/H48hDPg").await.context("initiating request")?;
        println!("{}", path);
        assert_eq!(path, "https://i.imgur.com/H48hDPg.gif");


        Ok(())
    }


    #[tokio::test]
    async fn imgur_gif_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

        let path = client.request("https://imgur.com/a/ErqxbAa").await.context("initiating request")?;
        assert_eq!(path, "https://i.imgur.com/su3kGzS.gif");

        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

        let path = client.request("https://imgur.com/gallery/rvGOIHS").await.context("initiating request")?;
        println!("{}", path);
        assert!(path.ends_with(".gif"));

        let path = format!("test-data/{}", path);

        let file = tokio::fs::read(&path).await.with_context(|| format!("Opening final gif path: {}", path))?;
        assert_ne!(file.len(), 0);


        Ok(())
    }

    #[tokio::test]
    async fn redgifs() -> Result<(), Error> {
        let client = Client::new("test-data".into(), None, None);

        let path = client.request("https://www.redgifs.com/watch/elderlysupportivepeafowl").await.context("initiating request")?;
        println!("{}", path);
        assert!(path.ends_with(".gif"));

        let path = "test-data/elderlysupportivepeafowl.gif";

        let file = tokio::fs::read(&path).await.with_context(|| format!("Opening final gif path: {}", path))?;
        assert_ne!(file.len(), 0);
        
        Ok(())
    }

    #[tokio::test]
    async fn parallel_request() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

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
            let task = tokio::spawn(async move {println!("started");let res = client.request(url).await; println!("done"); return res;});
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
                let res = tasks.remove(&url).ok_or(anyhow!("Task not found"))?.await??;
                resolved.insert(url, res);
            }

            // Give tokio a chance to run the tasks
            tokio::task::yield_now().await;
        }

        assert_eq!(resolved.len(), 3);
        let first_resp = resolved.get("https://imgur.com/gallery/rvGOIHS").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "rvGOIHS.gif".to_string());

        let first_resp = resolved.get("https://imgur.com/a/ErqxbAa").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "https://i.imgur.com/su3kGzS.gif".to_string());

        let first_resp = resolved.get("https://imgur.com/H48hDPg").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "https://i.imgur.com/H48hDPg.gif".to_string());

        Ok(())
    }
}