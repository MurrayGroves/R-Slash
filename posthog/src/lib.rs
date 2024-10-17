use std::collections::HashMap;

use anyhow::anyhow;
use log::*;
use serde_json::json;
use serde_json::Value;

#[derive(Clone, Debug)]
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

    pub async fn capture(
        &self,
        event: &str,
        properties: impl Into<serde_json::Value>,
        guild_id: Option<impl Into<u64>>,
        channel_id: Option<impl Into<u64>>,
        distinct_id: &str,
    ) -> Result<reqwest::Response, anyhow::Error> {
        let guild_id: Option<u64> = guild_id.map(|v| v.into());
        let channel_id: Option<u64> = channel_id.map(|v| v.into());

        let mut properties: serde_json::Value = properties.into();
        let mut groups = json!({});
        if let Some(channel_id) = channel_id {
            groups["channel"] = Value::String(channel_id.to_string());
        }
        if let Some(guild_id) = guild_id {
            groups["guild"] = Value::String(guild_id.to_string());
        }
        properties["$groups"] = groups;

        if distinct_id.starts_with("user_")
            || distinct_id.starts_with("guild_")
            || guild_id.is_some()
            || channel_id.is_some()
        {
            let flags = self.get_feature_flags(distinct_id, guild_id).await?;
            for (k, v) in flags {
                properties[format!("$feature/{}", k)] = v.into();
            }
        }

        let json = json!({
            "api_key": self.api_key,
            "distinct_id": distinct_id,
            "event": event,
            "properties": properties
        });
        let body = serde_json::to_string(&json).unwrap();
        debug!("{:?}", body);
        let res = self
            .client
            .post(format!("{}/capture", self.host))
            .body(body)
            .header("Content-Type", "application/json")
            .send()
            .await?;
        Ok(res)
    }

    pub async fn get_feature_flags(
        &self,
        distinct_id: &str,
        guild_id: Option<impl Into<u64>>,
    ) -> Result<HashMap<String, String>, anyhow::Error> {
        let mut groups = json!({});

        if let Some(guild_id) = guild_id {
            groups["guild"] = Value::String(guild_id.into().to_string());
        }

        let json = json!({
            "api_key": self.api_key,
            "distinct_id": distinct_id,
            "groups": groups
        });
        let body = serde_json::to_string(&json).unwrap();
        trace!("{:?}", body);
        let res = self
            .client
            .post(format!("{}/decide?v=3", self.host))
            .body(body)
            .header("Content-Type", "application/json")
            .send()
            .await?;

        let res = res.json::<Value>().await?;

        debug!("Flags response: {:?}", res);

        let flags: HashMap<String, Value> = serde_json::from_value(
            res.get("featureFlags")
                .map(|v| v.clone())
                .ok_or(anyhow!("featureFlags not present"))?,
        )?;

        let flags = flags
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    match v {
                        Value::String(s) => s.clone(),
                        _ => v.to_string(),
                    },
                )
            })
            .collect();

        debug!("Flags: {:?}", flags);

        Ok(flags)
    }
}
