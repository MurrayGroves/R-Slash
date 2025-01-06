use std::hash::{Hash, Hasher};

use anyhow::Error;
use dash_mpd::fetch::DashDownloader;
use tracing::instrument;

#[derive(Clone)]
pub struct Client {
	path: String,
	client: reqwest::Client,
	limiter: super::client::Limiter,
}

impl Client {
	pub fn new(path: String, limiter: super::client::Limiter) -> Self {
		Self {
			path,
			client: reqwest::Client::new(),
			limiter,
		}
	}

	#[instrument(skip(self))]
	pub async fn request(&self, url: &str, filename: &str) -> Result<String, Error> {
		self.limiter.wait().await;

		let c = DashDownloader::new(url)
			.with_http_client(self.client.clone())
			.prefer_video_height(300)
			.download_to(format!("{}/{}.mp4", self.path, filename)).await?;

		self.limiter.update().await;

		let path = c.to_str().ok_or(Error::msg("Failed to extract image link from Imgur response"))?.to_string();
		let path = path.replace(&format!("{}/", self.path), "");
		Ok(path)
	}
}