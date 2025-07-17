use anyhow::{Error, Result};
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use tokio::{io::AsyncWriteExt, sync::RwLock};
use tracing::{debug, instrument};

#[derive(Clone)]
pub struct Client {
    path: String,
    client: reqwest::Client,
    limiter: super::client::Limiter,
    token: Arc<RwLock<super::client::Token>>,
}

impl Client {
    pub async fn new(path: String, limiter: super::client::Limiter) -> Result<Self> {
        Ok(Self {
            path,
            client: reqwest::Client::builder().user_agent("Booty Bot").build()?,
            limiter,
            token: Arc::new(RwLock::new(
                super::client::Token::new("https://api.redgifs.com/v2/auth/temporary".to_string())
                    .await?,
            )),
        })
    }

    #[instrument(skip(self))]
    pub async fn request(&self, mut url: &str, filename: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        // Remove trailing slashes
        url = url.trim_end_matches('/');

        let mut id = url.split("/").last().ok_or(Error::msg("No ID in url"))?;
        id = id.split(".").next().ok_or(Error::msg("No ID in url"))?;

        let url = format!("https://api.redgifs.com/v2/gifs/{}", id);
        let mut token_lock = self.token.write().await;
        let token = token_lock.get_token().await?.to_string();
        drop(token_lock);

        let resp = match timeout(
            Duration::from_secs(30),
            self.client
                .get(&url)
                .header("authorization", format!("Bearer {}", token))
                .send(),
        )
        .await
        {
            Ok(req) => req?,
            Err(_) => {
                debug!("Request timed out, returning error");
                return Err(Error::msg("Request timed out"));
            }
        };

        self.limiter
            .update_headers(resp.headers(), resp.status())
            .await?;

        let json = resp.json::<serde_json::Value>().await?;
        debug!("Response Json {:?}", json);
        let download_url = json
            .get("gif")
            .ok_or_else(|| match json.get("error") {
                Some(e) => {
                    return if let Some(code) = e.get("code") {
                        if let Some(description) = e.get("description") {
                            debug!("Description: {:?}", description);
                            if description == "gif not ready" || description == "gif not found" {
                                // Ok I think technically this happens when the user clicks share before the gif is ready?? Idk they have different URLs in the wrong format too
                                return Error::msg("Deleted");
                            }
                        }

                        if code == "Gone" {
                            Error::msg("Deleted")
                        } else {
                            Error::msg(format!("Error: {}", e))
                        }
                    } else {
                        Error::msg(format!("Error: {}", e))
                    };
                }
                None => Error::msg("No gif in response"),
            })?
            .get("urls")
            .ok_or(Error::msg("No urls in response"))?
            .get("sd")
            .ok_or(Error::msg("No mp4 url in response"))?
            .as_str()
            .ok_or(Error::msg("mp4 url is not a string"))?;

        let path = format!("{}/{}.mp4", self.path, filename);
        debug!("Downloading {} to {}", download_url, path);
        self.limiter.wait().await;
        debug!("Finished waiting, sending request...");

        let req = match timeout(
            Duration::from_secs(30),
            self.client.get(download_url).send(),
        )
        .await
        {
            Ok(req) => req?,
            Err(_) => {
                debug!("Request timed out, returning error");
                return Err(Error::msg("Request timed out"));
            }
        };
        debug!("Request sent, updating headers...");
        self.limiter
            .update_headers(req.headers(), req.status())
            .await?;
        debug!("Headers updated, creating file...");
        let mut file = tokio::fs::File::create(&path).await?;
        let mut stream = req.bytes_stream();

        debug!("File created, starting to write stream...");
        while let Some(item) = match timeout(Duration::from_secs(10), stream.next()).await {
            Ok(item) => item,
            Err(_) => {
                debug!("Stream timed out, returning error");
                return Err(Error::msg("Stream timed out"));
            }
        } {
            let chunk = item?;
            file.write_all(&chunk).await?;
        }

        debug!("Downloaded...");
        debug!("Returning {}", path.replace(&format!("{}/", self.path), ""));

        Ok(path.replace(&format!("{}/", self.path), ""))
    }
}
