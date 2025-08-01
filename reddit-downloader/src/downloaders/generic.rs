use std::io::Write;

use crate::REDDIT_LIMITER;
use anyhow::{Context, Error};
use tracing::instrument;

#[derive(Clone)]
pub struct Client {
    path: String,
    client: reqwest::Client,
    limiter: rslash_common::Limiter,
}

impl Client {
    pub fn new(path: String, limiter: rslash_common::Limiter) -> Self {
        Self {
            path,
            client: reqwest::Client::builder()
                .cookie_store(true)
                .build()
                .unwrap(),
            limiter,
        }
    }

    /// Download a single mp4 from a url, and return the path to the mp4, or URL if no conversion is needed
    #[instrument(skip(self))]
    pub async fn request(&self, url: &str, filename: &str) -> Result<String, Error> {
        let write_path = format!("{}/{}", self.path, filename);

        if url.contains("reddit.com") {
            REDDIT_LIMITER.wait().await;
        } else {
            self.limiter.wait().await;
        }

        // Had issues with invalid requests using reqwest, so I just used wget :)
        let output = tokio::process::Command::new("wget")
            .arg("-O")
            .arg(&write_path)
            .arg(url)
            .output()
            .await?;

        if url.contains("reddit.com") {
            REDDIT_LIMITER.update().await;
        } else {
            self.limiter.update().await;
        }

        if !output.status.success() {
            std::io::stderr().write_all(&output.stderr)?;
            Err(Error::msg(format!(
                "wget failed: {}\n{}",
                output.status.to_string(),
                output.status
            )))
            .with_context(|| format!("path: {}", write_path))?;
        }

        Ok(filename.to_string())
    }
}
