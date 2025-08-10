use rslash_common::SubredditStatus;
use serde::{Deserialize, Serialize};

#[tarpc::service]
pub trait RedditProxy {
    async fn get(url: String) -> Result<String, RedditProxyError>;

    /// Check a subreddit's validity by issuing a HEAD request and checking if status code is 200
    async fn check_subreddit_valid(subreddit: String) -> Result<SubredditStatus, RedditProxyError>;
}

#[derive(thiserror::Error, Debug, Deserialize, Serialize)]
pub enum RedditProxyError {
    #[error("Error fetching Reddit token")]
    TokenError(String),

    #[error("Error sending/receiving the request from Reddit")]
    RequestError(String),

    #[error("Response could not be decoded into text")]
    TextDecodeError(String),

    #[error("Response could not be decoded into JSON")]
    JsonDecodeError(String),
}
