use std::{hash::{Hash, Hasher}, io::Write};

use anyhow::{Error, Context};
use tracing::debug;

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
    

    pub async fn request(&self, url: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let mut id = url.split("/").last().ok_or(Error::msg("No ID in url"))?;
        id = id.split(".").next().ok_or(Error::msg("No ID in url"))?;

        let url = format!("https://redgifs.com/ifr/{}", id);
        let html = self.client.get(&url).send().await?.text().await?;
        self.limiter.update().await;
        let meta = metascraper::MetaScraper::parse(&html)?.metadata().metatags.ok_or(Error::msg("Failed to parse metatags"))?;

        for tag in meta {
            if tag.name == "og:video" {
                let path = format!("{}/{}.mp4", self.path, id);
                self.limiter.wait().await;
                // Had issues with invalid requests using reqwest, so I just used wget :)
                let output = tokio::process::Command::new("wget")
                .arg("-r")
                .arg("-O")
                .arg(&path)
                .arg(tag.content)
                .output().await?;

                self.limiter.update().await;

                if !output.status.success() {
                    std::io::stderr().write_all(&output.stderr).unwrap();
                    Err(Error::msg(format!("wget failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", path))?;
                }
                return Ok(path.replace(&format!("{}/", self.path), ""));
            } else if tag.name == "og:image:url" {
                let path = format!("{}/{}.jpg", self.path, id);
                self.limiter.wait().await;
                let output = tokio::process::Command::new("wget")
                .arg("-r")
                .arg("-O")
                .arg(&path)
                .arg(tag.content)
                .output().await?;

                self.limiter.update().await;

                if !output.status.success() {
                    std::io::stderr().write_all(&output.stderr).unwrap();
                    Err(Error::msg(format!("wget failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", path))?;
                }
                return Ok(path.replace(&format!("{}/", self.path), ""));
            }
        }

        Err(Error::msg("Failed to find video link in metatags"))
    }
}