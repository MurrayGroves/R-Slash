use std::{hash::{Hash, Hasher}, io::Write};

use anyhow::{Error, Context, Result};
use tracing::debug;

pub struct Client<'a> {
    path: &'a str,
    client: reqwest::Client,
    limiter: super::client::Limiter,
    token: super::client::Token,
}

impl <'a>Client<'a> {
    pub async fn new(path: &'a str, limiter: super::client::Limiter) -> Result<Self> {
        Ok(Self {
            path,
            client: reqwest::Client::new(),
            limiter: limiter,
            token: super::client::Token::new("https://api.regifs.com/v2/auth/temporary".to_string()).await?,
        })
    }
    

    pub async fn request(&self, url: &str, filename: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let mut id = url.split("/").last().ok_or(Error::msg("No ID in url"))?;
        id = id.split(".").next().ok_or(Error::msg("No ID in url"))?;

        let url = format!("https://api.redgifs.com/v2/gifs/{}", id);
        let resp = self.client.get(&url).send().await?;
        self.limiter.update_headers(resp.headers()).await;

        let json = resp.json::<serde_json::Value>().await?;
        let download_url = json
                                    .get("urls").ok_or(Error::msg("No urls in response"))?
                                    .get("sd").ok_or(Error::msg("No mp4 url in response"))?
                                    .as_str().ok_or(Error::msg("mp4 url is not a string"))?;

        let path = format!("{}/{}.mp4", self.path, filename);
        self.limiter.wait().await;
        // Had issues with invalid requests using reqwest, so I just used wget :)
        let output = tokio::process::Command::new("wget")
        .arg("-r")
        .arg("-O")
        .arg(&path)
        .arg(download_url)
        .output().await?;

        self.limiter.update().await;

        if !output.status.success() {
            std::io::stderr().write_all(&output.stderr).unwrap();
            Err(Error::msg(format!("wget failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", path))?;
        }
        return Ok(path.replace(&format!("{}/", self.path), ""));
    }
}