use std::collections::HashMap;

use tracing::{debug, info, warn, error};

struct Client {
    
}

/// Request access token from Redgifs API, returns token and expiry time from epoch
pub async fn get_token(client: reqwest::Client, client_id: String, client_secret: String) -> Result<(String, u64), Box<dyn std::error::Error>> {
    let response = client.post("https://api.redgifs.com/v2/oauth/client")
        .form(&[("grant_type", "client_credentials"),
                ("client_id", &client_id),
                ("client_secret", &client_secret)])
        .send()
        .await?;

    if !(response.status() == 200) {
        let txt = "Failed to get token from Redgifs API, status code: ".to_owned() + &response.status().to_string();
        sentry::capture_message(&txt, sentry::Level::Warning);
        warn!("{}", txt);
        return Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other, txt)));
    }
    let response_json: serde_json::Value = response.json().await?;
    let token = response_json["access_token"].as_str().ok_or("access_token not present in response")?.to_owned();
    let expiry = response_json["expires_in"].as_u64().ok_or("expires_in not present in response")?;
    let expiry = chrono::Utc::now().timestamp() + expiry as i64;
    Ok((token, expiry as u64))
}


/// Get gif URLs by ID
/// Returns a hashmap of IDs to URLs, if the URL is None then the gif was not found on redgifs
async fn get_gif_urls(token: String, client: reqwest::Client, ids: Vec<String>) -> Result<HashMap<String, Option<String>>, Box<dyn std::error::Error>> {
    let url = "https://api.redgifs.com/v2/gifs?ids=".to_owned() + &ids.join(",");
    let response: serde_json::Value = client.get(url).send().await?.json().await?;

    let mut map = HashMap::new();
    for id in ids {
        map.insert(id, None);
    }

    for gif in response.get("gifs").ok_or("gifs not present in response")?.as_array().ok_or("gifs not an array")? {
        let url = gif.get("urls").ok_or("urls not present in response")?.get("sd").ok_or("mp4 not present in response")?.as_str().ok_or("mp4 not a string")?.to_owned();
        let id = gif.get("id").ok_or("id not present in response")?.as_str().ok_or("id not a string")?.to_owned();
        map.insert(id, Some(url));
    }

    Ok(map)
}

/// Download and process gifs by ID
/// Returns when all gifs are processed and ready for use
///
/// /// # API Usage
/// Doesn't implement rate limiting, must be done by caller (1000 requests per hour)
/// Uses one API call per request or 100 IDs per request

pub async fn download_gifs(token: String, client: reqwest::Client, ids: Vec<String>) {


}