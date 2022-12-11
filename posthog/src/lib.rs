use std::collections::HashMap;

use log::*;
use serde_json::json;

pub struct Client {
    pub api_key: String,
    pub host: String,
    pub client: reqwest::Client,
}

impl Client {
    pub fn new(api_key: String, host: String) -> Client {
        Client {
            api_key,
            host,
            client: reqwest::Client::new(),
        }
    }

    pub async fn capture(&self, event: &str,  properties: Option<HashMap<&str, String>>, distinct_id: &str) -> Result<reqwest::Response, reqwest::Error> {
        let mut props = HashMap::new();
        props.insert("distinct_id", distinct_id.to_string());
        if let Some(properties) = properties {
            for (key, value) in properties {
                props.insert(key, value);
            }
        }

        let json = json!({
            "api_key": self.api_key,
            "event": event,
            "properties": props,
        });
        let body = serde_json::to_string(&json).unwrap();
        debug!("{:?}", body);
        let res = self.client.post(&self.host).body(body).header("Content-Type", "application/json").send().await?;
        Ok(res)
    }
}