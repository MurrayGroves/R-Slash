use std::process::Command;

mod redgifs;
mod imgur;
mod generic;


/// Client for downloading mp4s from various sources and converting them to gifs for embedding
struct Client {
    redgifs: redgifs::Client,
    imgur: imgur::Client,
    generic: generic::Client,
}

impl Client {
    /// # Arguments
    /// * `path` - Path to the directory where the downloaded files will be stored
    pub fn new(path: &str) -> Self {
        Self {
            redgifs: redgifs::Client::new(path),
            imgur: imgur::Client::new(path),
            generic: generic::Client::new(path),
        }
    }

    /// Convert a single mp4 to a gif, and return the path to the gif
    async fn process(&self, path: &str) -> Result<String, Box<dyn std::error::Error>> {
        let f = std::fs::File::open(path)?;
        let size = f.metadata()?.len();
        let reader = std::io::BufReader::new(f);
        let mp4 = mp4::Mp4Reader::read_header(reader, size)?;
        let track = mp4.tracks().next()?;
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

        let new_path = format!("{}.gif", path.replace(".mp4", ""));

        let status = Command::new("ffmpeg")
            .arg("-i")
            .arg(path)
            .arg("-vf")
            .arg(format!("\"fps=15{}\",split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse\"", scale))
            .arg("-loop")
            .arg("0")
            .arg(new_path)
            .output()
            .status()?;

        if !status.success() {
            return Err("ffmpeg failed: {}\n{}", status.to_string(), String::from_utf8_lossy(&status.stderr).to_string());
        }

        Ok(new_path)
    }

    /// Convert a batch of mp4s to gifs, and return a map of mp4 paths to gif paths
    async fn process_batch(&self, paths: HashMap<String, String>) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
        let mut gifs = HashMap::new();

        for (url, path) in paths {
            gifs.insert(url, self.process(path).await?);
        };

        Ok(gifs)
    }

    /// Download a single mp4 from a url, and return the path to the mp4
    pub async fn request(&self, url: &str) -> Result<String, Box<dyn std::error::Error>> {
        let path = if url.contains("redgifs.com") {
            self.redgifs.request(url).await?
        } else if url.contains("imgur.com") {
            self.imgur.request(url).await?
        } else {
            self.generic.request(url).await?
        };

        return self.process(path).await;
    }

    /// Download a batch of mp4s from urls, and return a map of urls to mp4 paths
    pub async fn request_batch(&self, urls: Vec<&str>) -> Result<HashMap<String, Option<String>>, Box<dyn std::error::Error>> {
        let mut redgifs = Vec::new();
        let mut imgur = Vec::new();
        let mut generic = Vec::new();

        for url in urls {
            if url.contains("redgifs.com") {
                redgifs.push(url);
            } else if url.contains("imgur.com") {
                imgur.push(url);
            } else {
                generic.push(url);
            }
        }

        let mut paths = HashMap::new();

        if !redgifs.is_empty() {
            paths.extend(self.redgifs.request_batch(redgifs).await?.into_iter());
        }
        
        if !imgur.is_empty() {
            paths.extend(self.imgur.request_batch(imgur).await?.into_iter());
        }

        if !generic.is_empty() {
            paths.extend(self.generic.request_batch(generic).await?.into_iter());
        }

        return self.process_batch(paths).await;
    }
}