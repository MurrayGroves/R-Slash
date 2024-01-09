use std::{collections::HashMap, sync::Arc};

use anyhow::{Error, anyhow, Context, Result};

use futures_util::StreamExt;

pub struct Client<'a> {
    path: &'a str,
    client_id: Arc<String>,
    client: reqwest::Client,
    limiter: super::client::Limiter,
}

impl <'a>Client<'a> {
    pub fn new(path: &'a str, client_id: Arc<String>, limiter: super::client::Limiter) -> Self {
        Self {
            path,
            client_id: client_id,
            client: reqwest::Client::new(),
            limiter: limiter,
        }
    }
    

    async fn request_gallery(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let response = self.client.get(&format!("https://api.imgur.com/3/gallery/{}", id))
            .header("Authorization", format!("Client-ID {}", self.client_id))
            .send()
            .await?;

        self.limiter.update_headers(response.headers()).await?;

        let response = response
            .json::<serde_json::Value>()
            .await.context("failed json decode when requesting gallery")?;

        Ok(response["data"]["images"][0]["link"].as_str().ok_or(Error::msg("Failed to extract image link from Imgur response"))?.to_string())
    }

    async fn request_album(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;

        let response = self.client.get(&format!("https://api.imgur.com/3/album/{}", id))
            .header("Authorization", format!("Client-ID {}", self.client_id))
            .send()
            .await?;

        self.limiter.update_headers(response.headers()).await?;

        let txt = response.text().await?;
        let json = serde_json::from_str::<serde_json::Value>(&txt).context(txt)?;

        Ok(json["data"]["images"][0]["link"].as_str().ok_or(Error::msg("Failed to extract image link from Imgur response"))?.to_string())
    }

    async fn request_image(&self, id: &str) -> Result<String, Error> {
        self.limiter.wait().await;
        let gif = format!("https://i.imgur.com/{}.gif", id);
        let head = self.client.head(&gif).send().await?;
        self.limiter.update_headers(head.headers()).await?;

        if head.headers().get("Content-Type").ok_or(Error::msg("Failed to get content type"))?.to_str()? == "image/gif" {
            return Ok(gif);
        }

        let mp4 = format!("https://i.imgur.com/{}.mp4", id);
        let exists = self.client.head(&mp4).send().await?.status().is_success();
        if exists {
            return Ok(mp4);
        }

        Ok(format!("https://i.imgur.com/{}.jpg", id))
    }


    /// Download a single image from a url, and return the path to the image, or URL if no conversion is needed
    pub async fn request(&self, url: &str) -> Result<String, Error> {
        let id = url.split("/").last().ok_or(Error::msg("Failed to extract imgur ID"))?
        .split(".").next().ok_or(Error::msg("Failed to extract imgur ID"))?;

        let download_url = if url.contains("/gallery/") {
            self.request_gallery(id).await?
        } else if url.contains("/a/") || url.contains("/album/") {
            self.request_album(id).await?
        } else {
            self.request_image(id).await?
        };

        // JPGs, PNGs and GIFs dont need to be converted
        if download_url.ends_with(".jpg") || download_url.ends_with(".png") || download_url.ends_with(".gif") {
            return Ok(download_url);
        }

        let resp = self.client.get(&download_url).send().await?;

        let write_path = format!("{}/{}.mp4", self.path, id);

        let mut file = tokio::fs::File::create(&write_path).await?;
        let mut stream = resp.bytes_stream();
        while let Some(item) = stream.next().await {
            tokio::io::copy(&mut item?.as_ref(), &mut file).await?;
        }

        Ok(format!("{}.mp4", id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_for_file_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Gallery that contains a GIF
        let path = format!("test-data/{}", client.request("https://imgur.com/gallery/bXw2p90").await?);
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Gallery that contains a JPG
        let path = client.request("https://imgur.com/gallery/u1tFRrk").await?;
        assert_eq!(path, "https://i.imgur.com/GUvTNyl.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_file_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Album that contains a GIF
        let path = format!("test-data/{}", client.request("https://imgur.com/a/Xz70QCa").await?);
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_album() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Album that contains a JPG
        let path = client.request("https://imgur.com/a/w6uDYLW").await?;
        assert_eq!(path, "https://i.imgur.com/LIcehKQ.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_link_album_gif() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Album that contains a JPG
        let path = client.request("https://imgur.com/a/ErqxbAa").await?;
        assert_eq!(path, "https://i.imgur.com/su3kGzS.gif");

        Ok(())
    }


    #[tokio::test]
    async fn check_for_link_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Single JPG image
        let path = client.request("https://imgur.com/GUvTNyl").await?;
        assert_eq!(path, "https://i.imgur.com/GUvTNyl.jpg");

        Ok(())
    }

    #[tokio::test]
    async fn check_for_file_image() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data", Arc::new(client_id), super::super::client::Limiter::new(Some(60)));
        // Single GIF image
        let path = format!("test-data/{}", client.request("https://imgur.com/K2sN8du").await?);
        let file = tokio::fs::read(&path).await?;
        assert_ne!(file.len(), 0);

        Ok(())
    }
}