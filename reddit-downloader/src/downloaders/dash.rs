use std::hash::{Hash, Hasher};

use anyhow::Error;
use dash_mpd::fetch::DashDownloader;
use tracing::instrument;

pub struct Client<'a> {
    path: &'a str,
    client: reqwest::Client,
    limiter: super::client::Limiter,
}

impl <'a>Client<'a> {
    pub fn new(path: &'a str, limiter: super::client::Limiter) -> Self {
        Self {
            path,
            client: reqwest::Client::new(),
            limiter: limiter,
        }
    }
    
    #[instrument(skip(self))]
    pub async fn request(&self, url: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        url.hash(&mut hasher);
        let hash = hasher.finish();

        let c = DashDownloader::new(url)
            .with_http_client(self.client.clone())
            .prefer_video_height(300)
            .download_to(format!("{}/{}.mp4", self.path, hash)).await?;

        self.limiter.update().await;

        let path = c.to_str().ok_or(Error::msg("Failed to extract image link from Imgur response"))?.to_string();
        let path = path.replace(&format!("{}/", self.path), "");
        Ok(path)
    }
}