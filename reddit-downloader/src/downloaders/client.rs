use std::collections::HashMap;
use std::io::Write;
use std::process::Command;

use anyhow::Error;

use super::redgifs;
use super::imgur;
use super::generic;


/// Client for downloading mp4s from various sources and converting them to gifs for embedding
pub struct Client<'a> {
    redgifs: redgifs::Client,
    imgur: Option<imgur::Client<'a>>,
    generic: generic::Client,
    posthog: Option<posthog::Client>,
    path: &'a str,
}

impl <'a>Client<'a> {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    /// * `imgur_client_id` - Client ID for the Imgur API, if missing then Imgur downloads will fail
    /// * `posthog_client` - Optional Posthog client for tracking downloads and api usage
    pub fn new(path: &'static str, imgur_client_id: Option<&'static str>, posthog_client: Option<posthog::Client>) -> Self {
        Self {
            redgifs: redgifs::Client::new(&path),
            imgur: match imgur_client_id {
                Some(client_id) => Some(imgur::Client::new(&path, client_id)),
                None => None,
            },
            generic: generic::Client::new(&path),
            path: &path,
            posthog: posthog_client
        }
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    async fn process(&self, path: &str) -> Result<String, Error> {
        let f = std::fs::File::open(format!("{}/{}", self.path, path))?;
        let size = f.metadata()?.len();
        let reader = std::io::BufReader::new(f);
        let mp4 = mp4::Mp4Reader::read_header(reader, size)?;
        let track = mp4.tracks().iter().next().ok_or(Error::msg("no mp4 tracks"))?.1;
        let width = track.width();
        let height = track.height();

        let path = format!("{}/{}", self.path, path);

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

        let new_path = format!("{}.gif", path.replace(".mp4", ""));

        let output = Command::new("ffmpeg")
            .arg("-y")
            .arg("-i")
            .arg(path)
            .arg("-vf")
            .arg(format!("fps=fps=15{},split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse", scale))
            .arg("-loop")
            .arg("0")
            .arg(&new_path)
            .output()?;

        if !output.status.success() {
            std::io::stderr().write_all(&output.stderr).unwrap();
            Err(Error::msg(format!("ffmpeg failed: {}\n{}", output.status.to_string(), output.status)))?;
        }

        Ok(new_path)
    }

    /// Download a single mp4 from a url, and return the path to the mp4
    pub async fn request(&self, url: &str) -> Result<String, Error> {
        let path = if url.contains("redgifs.com") {
            self.redgifs.request(url).await?
        } else if url.contains("imgur.com") {
            match &self.imgur {
                Some(imgur) => imgur.request(url).await?,
                None => Err(Error::msg("Imgur client ID not set"))?,
            }
        } else {
            self.generic.request(url).await?
        };

        return self.process(&path).await;
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(path, "test-data/test.gif");
        Ok(())
    }
}