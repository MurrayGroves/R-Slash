use std::collections::HashMap;
use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Error, anyhow};

use super::redgifs;
use super::imgur;
use super::generic;


/// Client for downloading mp4s from various sources and converting them to gifs for embedding
pub struct Client<'a> {
    redgifs: redgifs::Client<'a>,
    imgur: Option<imgur::Client<'a>>,
    generic: generic::Client<'a>,
    posthog: Option<posthog::Client>,
    path: &'a str,
}

impl <'a>Client<'a> {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    /// * `imgur_client_id` - Client ID for the Imgur API, if missing then Imgur downloads will fail
    /// * `posthog_client` - Optional Posthog client for tracking downloads and api usage
    pub fn new(path: &'a str, imgur_client_id: Option<String>, posthog_client: Option<posthog::Client>) -> Self {
        Self {
            redgifs: redgifs::Client::new(&path),
            imgur: match imgur_client_id {
                Some(client_id) => Some(imgur::Client::new(&path, Arc::new(client_id))),
                None => None,
            },
            generic: generic::Client::new(&path),
            path: &path,
            posthog: posthog_client
        }
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    async fn process(&self, path: &str) -> Result<String, Error> {
        let new_path: String = format!("{}.gif", path.replace(".mp4", ""));
        let full_path = format!("{}/{}", self.path, path);
        let new_full_path = format!("{}/{}", self.path, new_path);

        let f = std::fs::File::open(&full_path).with_context(|| format!("Opening mp4 path {}", &full_path))?;
        let size = f.metadata()?.len();
        let reader = std::io::BufReader::new(f);
        let mp4 = mp4::Mp4Reader::read_header(reader, size).with_context(|| format!("reading mp4 header {}", full_path))?;
        let track = mp4.tracks().iter().next().ok_or(Error::msg("no mp4 tracks"))?.1;
        let width = track.width();
        let height = track.height();



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
        };

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
            Err(Error::msg(format!("ffmpeg failed: {}\n{}", output.status.to_string(), output.status))).with_context(|| format!("path: {}", full_path))?;
        }

        Ok(new_path)
    }

    /// Download a single mp4 from a url, and return the path to the mp4
    pub async fn request(&self, url: &str) -> Result<String, Error> {
        let path = if url.contains("redgifs.com") {
            self.redgifs.request(url).await?
        } else if url.contains("imgur.com") {
            match &self.imgur {
                Some(imgur) => imgur.request(url).await.context("Requesting from imgur")?,
                None => Err(Error::msg("Imgur client ID not set"))?,
            }
        } else {
            self.generic.request(url).await?
        };

        if path.ends_with(".mp4") {
            return self.process(&path).await.context("Processing gif");
        } else {
            return Ok(path);
        }

    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, pin::Pin};

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

        let path = client.request("https://imgur.com/a/iv1rtf1").await.context("initiating request")?;
        assert_eq!(path, "https://i.imgur.com/H48hDPg.gif");

        Ok(())
    }

    #[tokio::test]
    async fn imgur_gif_gallery() -> Result<(), Error> {
        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

        let path = client.request("https://imgur.com/gallery/bXw2p90").await.context("initiating request")?;
        println!("{}", path);
        assert!(path.ends_with(".gif"));

        let path = format!("test-data/{}", path);

        let file = tokio::fs::read(&path).await.with_context(|| format!("Opening final gif path: {}", path))?;
        assert_ne!(file.len(), 0);


        Ok(())
    }

    #[tokio::test]
    async fn parallel_request() -> Result<(), Error> {
        //console_subscriber::init();

        let client_id = std::env::var("IMGUR_CLIENT_ID")?;
        let client = Client::new("test-data".into(), Some(client_id), None);

        let urls = vec![
            "https://imgur.com/gallery/bXw2p90",
            "https://imgur.com/a/iv1rtf1",
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
        let first_resp = resolved.get("https://imgur.com/gallery/bXw2p90").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "bXw2p90.gif".to_string());

        let first_resp = resolved.get("https://imgur.com/a/iv1rtf1").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "https://i.imgur.com/H48hDPg.gif".to_string());

        let first_resp = resolved.get("https://imgur.com/H48hDPg").ok_or(anyhow!("No response"))?.to_string();
        assert_eq!(first_resp, "https://i.imgur.com/H48hDPg.gif".to_string());

        Ok(())
    }
}