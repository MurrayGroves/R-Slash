use std::sync::Arc;

use anyhow::{Context, Error, Result, anyhow, bail};

use futures_util::StreamExt;
use tracing::{debug, instrument};

#[derive(Clone)]
pub struct Client {
    path: String,
    client_id: Arc<String>,
    client: reqwest::Client,
    limiter: rslash_common::Limiter,
}

impl Client {
    pub fn new(path: String, client_id: Arc<String>, limiter: rslash_common::Limiter) -> Self {
        Self {
            path,
            client_id,
            client: reqwest::Client::new(),
            limiter,
        }
    }

    #[instrument(skip(self))]
    async fn request_gallery(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let response = self
            .client
            .get(&format!("https://api.imgur.com/3/gallery/{}", id))
            .header("Authorization", format!("Client-ID {}", self.client_id))
            .send()
            .await?;

        self.limiter
            .update_headers(response.headers(), response.status())
            .await?;

        if response.status() == 404 {
            bail!("Deleted")
        }

        let response = response
            .json::<serde_json::Value>()
            .await
            .context("failed json decode when requesting gallery")?;

        debug!("Imgur gallery response: {:?}", response);
        Ok(response["data"]["images"][0]["link"]
            .as_str()
            .ok_or(Error::msg(
                "Failed to extract image link from Imgur response",
            ))?
            .to_string())
    }

    #[instrument(skip(self))]
    async fn request_album(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let response = self
            .client
            .get(&format!("https://api.imgur.com/3/album/{}", id))
            .header("Authorization", format!("Client-ID {}", self.client_id))
            .send()
            .await?;

        self.limiter
            .update_headers(response.headers(), response.status())
            .await?;

        if response.status() == 404 {
            bail!("Deleted")
        }

        let txt = response.text().await?;
        let json = match serde_json::from_str::<serde_json::Value>(&txt) {
            Ok(json) => json,
            Err(e) => {
                debug!("Imgur album response for id {}: {}", id, txt);
                bail!(e);
            }
        };

        Ok(json["data"]["images"][0]["link"]
            .as_str()
            .ok_or(Error::msg(
                "Failed to extract image link from Imgur response",
            ))?
            .to_string())
    }

    #[instrument(skip(self))]
    async fn request_image(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let response = self
            .client
            .get(&format!("https://api.imgur.com/3/image/{}", id))
            .header("Authorization", format!("Client-ID {}", self.client_id))
            .send()
            .await?;

        self.limiter
            .update_headers(response.headers(), response.status())
            .await?;

        if response.status() == 404 {
            bail!("Deleted")
        }

        let txt = response.text().await?;
        let json = match serde_json::from_str::<serde_json::Value>(&txt) {
            Ok(json) => json,
            Err(e) => {
                debug!("Imgur image response for id {}: {}", id, txt);
                bail!(e);
            }
        };

        Ok(json["data"]["link"]
            .as_str()
            .ok_or(Error::msg(
                "Failed to extract image link from Imgur response",
            ))?
            .to_string())
    }

    /// Download a single image from a url, and return the path to the image, or URL if no conversion is needed
    #[instrument(skip(self))]
    pub async fn request(&self, mut url: &str, filename: &str) -> Result<String, Error> {
        if url.ends_with("/") {
            url = &url[..url.len() - 1];
        }

        let id = url
            .split("/")
            .last()
            .ok_or(Error::msg("Failed to extract imgur ID"))?
            .split(".")
            .next()
            .ok_or(Error::msg("Failed to extract imgur ID"))?;

        let id = id
            .split("-")
            .last()
            .ok_or(anyhow!("Failed to extract imgur ID"))?;

        let download_url = if url.contains("/a/") || url.contains("/album/") {
            self.request_album(id).await?
        } else if url.contains("/gallery/") {
            self.request_gallery(id).await?
        } else {
            self.request_image(id).await?
        };

        // JPGs, PNGs and GIFs dont need to be converted
        if download_url.ends_with(".jpg")
            || download_url.ends_with(".png")
            || download_url.ends_with(".gif")
        {
            return Ok(download_url);
        }

        self.limiter.wait().await;
        let resp = self.client.get(&download_url).send().await?;
        self.limiter
            .update_headers(resp.headers(), resp.status())
            .await?;

        let write_path = format!("{}/{}.mp4", self.path, filename);

        let mut file = tokio::fs::File::create(&write_path).await?;
        let mut stream = resp.bytes_stream();
        while let Some(item) = stream.next().await {
            tokio::io::copy(&mut item?.as_ref(), &mut file).await?;
        }

        Ok(format!("{}.mp4", filename))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_for_file_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Gallery that contains a GIF
        let path = format!(
            "test-data/{}",
            client
                .request("https://imgur.com/gallery/bXw2p90", "test")
                .await?
        );
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Gallery that contains a JPG
        let path = client
            .request("https://imgur.com/gallery/u1tFRrk", "test")
            .await?;
        assert_eq!(path, "https://i.imgur.com/GUvTNyl.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_file_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Album that contains a GIF
        let path = format!(
            "test-data/{}",
            client
                .request("https://imgur.com/a/Xz70QCa", "test")
                .await?
        );
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Album that contains a JPG
        let path = client
            .request("https://imgur.com/a/w6uDYLW", "test")
            .await?;
        assert_eq!(path, "https://i.imgur.com/LIcehKQ.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_album_gif() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Album that contains a JPG
        let path = client
            .request("https://imgur.com/a/ErqxbAa", "test")
            .await?;
        assert_eq!(path, "https://i.imgur.com/su3kGzS.gif");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Single JPG image
        let path = client.request("https://imgur.com/GUvTNyl", "test").await?;
        assert_eq!(path, "https://i.imgur.com/GUvTNyl.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_file_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new(
            "test-data".to_string(),
            Arc::new(client_id),
            rslash_common::Limiter::new(Some(60), "imgur".to_string()),
        );
        // Single GIF image
        let path = format!(
            "test-data/{}",
            client.request("https://imgur.com/K2sN8du", "test").await?
        );
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }
}
