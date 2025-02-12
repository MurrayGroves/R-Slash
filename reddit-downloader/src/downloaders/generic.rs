use std::{collections::HashMap, process::Command, io::Write};

use anyhow::{Error, Context};
use futures_util::StreamExt;
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
			client: reqwest::Client::builder().cookie_store(true).build().unwrap(),
			limiter,
		}
	}

	/// Download a single mp4 from a url, and return the path to the mp4, or URL if no conversion is needed
	#[instrument(skip(self))]
	pub async fn request(&self, url: &str, filename: &str) -> Result<String, Error> {
		let id = url.split("/").last().ok_or(Error::msg("No ID in url"))?;
		let write_path = format!("{}/{}", self.path, filename);

		self.limiter.wait().await;

		// Had issues with invalid requests using reqwest, so I just used wget :)
		let output = tokio::process::Command::new("wget")
			.arg("-O")
			.arg(&write_path)
			.arg(url)
			.output().await?;

		self.limiter.update().await;

		if !output.status.success() {
			std::io::stderr().write_all(&output.stderr).unwrap();
			Err(Error::msg(format!("wget failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", write_path))?;
		}

		Ok(filename.to_string())
	}

	pub async fn request_batch(&self, urls: Vec<&str>) -> Result<HashMap<String, String>, Error> {
		todo!();
	}
}