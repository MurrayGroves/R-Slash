use anyhow::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::Level;

use reqwest::{header, StatusCode};
use tokio::time::timeout;
use tracing::{debug, info, trace, warn};

use lazy_static::lazy_static;
use redis::AsyncTypedCommands;
use reqwest::header::HeaderMap;
use std::time::{SystemTime, UNIX_EPOCH};
use metrics::{counter, histogram};

lazy_static! {
    static ref REDDIT_LIMITER: Limiter = Limiter::new(None, "reddit".to_string());
}

/// Returns current milliseconds since the Epoch
fn get_epoch_ms() -> Result<u64, Error> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}

/// Request an access token from Reddit
///
/// Returns the token and the Unix timestamp at which it expires.
/// # Arguments
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## web_client
/// The [web_client](reqwest::Client) to make requests with.
/// ## device_id
/// If specified, will request and [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
#[tracing::instrument]
async fn request_reddit_access_token(
    reddit_client: &str,
    reddit_secret: &str,
    web_client: Option<&reqwest::Client>,
    device_id: Option<String>,
) -> Result<(String, u64), Error> {
    let web_client = match web_client {
        Some(x) => x,
        None => {
            let mut default_headers = HeaderMap::new();
            default_headers.insert(
                header::COOKIE,
                header::HeaderValue::from_static("_options={%22pref_gated_sr_optin%22:true}"),
            );

            &reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .default_headers(default_headers)
                .user_agent(format!(
                    "Discord:RSlash:{} (by /u/murrax2)",
                    env!("CARGO_PKG_VERSION")
                ))
                .build()?
        }
    };

    let post_data = match device_id {
        Some(x) => format!(
            "grant_type=https://oauth.reddit.com/grants/installed_client&\\device_id={}",
            x
        ),
        None => "grant_type=client_credentials".to_string(),
    };

    REDDIT_LIMITER.wait().await;

    let res = web_client
        .post("https://www.reddit.com/api/v1/access_token")
        .body(post_data)
        .basic_auth(reddit_client, Some(reddit_secret))
        .send()
        .await?;

    REDDIT_LIMITER
        .update_headers(res.headers(), res.status())
        .await?;

    let text = match res.text().await {
        Ok(x) => x,
        Err(x) => {
            let txt = format!("Failed to get text from reddit: {}", x);
            warn!("{}", txt);
            sentry::capture_message(&txt, sentry::Level::Warning);
            return Err(x)?;
        }
    };
    debug!("Matched text");
    let results: serde_json::Value = match serde_json::from_str(&text) {
        Ok(x) => x,
        Err(x) => {
            let txt = format!("Failed to parse JSON from Reddit: {}", text);
            warn!("{}", txt);
            sentry::capture_message(&txt, sentry::Level::Warning);
            return Err(x)?;
        }
    };

    debug!("Reddit access token response: {:?}", results);
    let token = results
        .get("access_token")
        .expect("Reddit did not return access token")
        .to_string();
    let expires_in: u64 = results
        .get("expires_in")
        .expect("Reddit did not provide expires_in")
        .as_u64()
        .unwrap();
    let expires_at = get_epoch_ms()? + expires_in * 1000;

    debug!(
        "New token expires in {} seconds, or at {}",
        expires_in, expires_at
    );

    Ok((token, expires_at))
}

/// Returns a String of the Reddit access token to use
///
/// # Arguments
/// ## con
/// The [connection](redis::aio::MultiplexedConnection) to the redis DB.
/// ## reddit_client
/// The bot's Reddit client_id
/// ## reddit_secret
/// The bot's secret
/// ## device_id
/// If specified, will request an [installed_client](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token instead of a [client_credentials](https://github.com/reddit-archive/reddit/wiki/OAuth2#application-only-oauth) token.
#[tracing::instrument(skip(con, web_client))]
pub async fn get_reddit_access_token(
    con: &mut redis::aio::MultiplexedConnection,
    reddit_client: &str,
    reddit_secret: &str,
    web_client: Option<&reqwest::Client>,
    device_id: Option<String>,
) -> Result<String, Error> {
    // Return a reddit access token
    let token_name = match device_id.clone() {
        // The key to grab from the DB
        Some(x) => x,
        _ => "default".to_string(),
    };

    let token = con.hget("reddit_tokens", token_name.clone()).await;
    let token = match token {
        Ok(Some(x)) => x,
        _ => {
            debug!("Requesting new access token, none exists");
            let token_results = request_reddit_access_token(
                reddit_client,
                reddit_secret,
                web_client,
                device_id.clone(),
            )
            .await?;

            let access_token = token_results.0;
            let expires_at = token_results.1;

            con.hset(
                "reddit_tokens",
                token_name.clone(),
                format!("{},{}", access_token, expires_at),
            )
            .await?;

            format!("{},{}", access_token, expires_at)
        }
    };

    let expires_at: u64 = token.split(",").collect::<Vec<&str>>()[1].parse()?;
    let mut access_token: String = token.split(",").collect::<Vec<&str>>()[0]
        .parse::<String>()?
        .replace('"', "");

    if expires_at < get_epoch_ms()? {
        debug!("Requesting new access token, current one expired");
        let token_results = request_reddit_access_token(
            reddit_client.clone(),
            reddit_secret.clone(),
            web_client,
            device_id.clone(),
        )
        .await?;
        access_token = token_results.0;
        let expires_at = token_results.1;

        con.hset(
            "reddit_tokens",
            token_name,
            format!("{},{}", access_token, expires_at),
        )
        .await?;
    }

    debug!("Reddit Token: {}", access_token.replace("\"", ""));
    Ok(access_token)
}

struct LimitState {
    remaining: u64,
    reset: i64, // Unix timestamp of when the limit resets
}

// Struct for tracking rate limits
#[derive(Clone)]
pub struct Limiter {
    state: Arc<Mutex<LimitState>>,
    per_minute: Option<u64>, // User can specify rate limit per minute, otherwise use the headers
    name: String,
}

impl Limiter {
    pub fn new(per_minute: Option<u64>, name: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(LimitState {
                remaining: 1,
                reset: 0,
            })),
            name,
            per_minute,
        }
    }

    // Update the rate limit based on the headers from the last request
    pub async fn update_headers(
        &self,
        headers: &reqwest::header::HeaderMap,
        status: StatusCode,
    ) -> Result<(), Error> {
        let mut state = self.state.lock().await;

        trace!(
            "Updating rate limit headers for {}: {:?}",
            self.name,
            headers
        );

        // If this limiter is using a per minute limit
        if let Some(per_minute) = self.per_minute {
            if state.remaining != 0 {
                state.remaining -= 1
            };
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }

            if let Some(remaining) = headers.get("Retry-After") {
                state.reset =
                    chrono::Utc::now().timestamp() + remaining.to_str()?.parse::<i64>()?;
                state.remaining = 0;
            }
        } else {
            if let Some(remaining) = headers.get("X-RateLimit-UserRemaining") {
                state.remaining = remaining.to_str()?.parse()?;
            } else if let Some(reset) = headers.get("X-RateLimit-UserReset") {
                state.reset = reset.to_str()?.parse()?;
            } else {
                if status == StatusCode::TOO_MANY_REQUESTS
                    || status == StatusCode::FORBIDDEN
                    || status == StatusCode::UNAUTHORIZED
                {
                    // Wait 10 minutes if we hit the rate limit, and they didn't tell us when to try again
                    warn!(
                        "We hit the rate limit for {} with status {} waiting 10 mins as a pre-caution with headers: {:?}",
                        self.name, status, headers
                    );
                    state.reset = chrono::Utc::now().timestamp() + 600;
                    state.remaining = 0;
                }

                if let Some(remaining) = headers.get("x-ratelimit-remaining") {
                    state.remaining = remaining
                        .to_str()?
                        .split(".")
                        .nth(0)
                        .ok_or(Error::msg("Failed to parse rate limit"))?
                        .parse()?;
                }
                if let Some(reset) = headers.get("x-ratelimit-reset") {
                    state.reset =
                        chrono::Utc::now().timestamp() + reset.to_str()?.parse::<i64>()?;
                }
            }
        }

        Ok(())
    }

    pub async fn update(&self) {
        let mut state = self.state.lock().await;
        if !state.remaining == 0 {
            state.remaining -= 1;
        }
        if let Some(per_minute) = self.per_minute {
            if state.reset < chrono::Utc::now().timestamp() {
                state.remaining = per_minute;
                state.reset = chrono::Utc::now().timestamp() + 60;
            }
        }
    }

    // Wait until we are allowed to request again
    #[tracing::instrument(skip(self))]
    pub async fn wait(&self) {
        let state = match timeout(Duration::from_secs(300), self.state.lock()).await {
            Ok(state) => state,
            Err(_) => {
                panic!("Failed to acquire lock on rate limit state within 5 minutes");
            }
        };
        if state.remaining < 3 && state.reset > chrono::Utc::now().timestamp() {
            let wait = state.reset - chrono::Utc::now().timestamp();
            counter!(format!("ratelimit_{}_hit", self.name)).increment(1);
            histogram!(format!("ratelimit_{}_wait", self.name)).record(wait as f64);
            drop(state);
            info!("Waiting {} seconds for rate limit on {}", wait, self.name);
            tokio::time::sleep(Duration::from_secs(wait as u64)).await;
            debug!("Finished waiting for rate limit on {}", self.name);
        }
    }
}
