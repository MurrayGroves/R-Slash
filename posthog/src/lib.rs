#![feature(duration_constructors_lite)]

use anyhow::anyhow;
use log::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

const CACHE_DURATION: Duration = Duration::from_hours(12);

#[derive(Clone, Debug)]
pub struct Client {
    pub api_key: Arc<String>,
    pub host: Arc<String>,
    pub client: reqwest::Client,
    cache: Arc<RwLock<HashMap<u64, FeatureFlags>>>,
}

#[derive(Debug)]
struct FeatureFlags {
    flags: HashMap<String, String>,
    last_fetched: Instant,
}

impl Client {
    pub fn new(api_key: String, host: String) -> Client {
        Client {
            api_key: Arc::new(api_key),
            host: Arc::new(host),
            client: reqwest::Client::new(),
            cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Send an event to Posthog
    pub async fn capture(
        &self,
        event: &str,
        properties: impl Into<Value>,
        guild_id: Option<impl Into<u64>>,
        channel_id: Option<impl Into<u64>>,
        distinct_id: &str,
    ) -> Result<reqwest::Response, anyhow::Error> {
        let guild_id: Option<u64> = guild_id.map(|v| v.into());
        let channel_id: Option<u64> = channel_id.map(|v| v.into());

        let mut properties: Value = properties.into();
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
            trace!("Getting feature flags");
            let flags = self.get_feature_flags(distinct_id, guild_id).await?;
            for (k, v) in flags {
                properties[format!("$feature/{}", k)] = v.into();
            }
        }

        let json = json!({
            "api_key": &*self.api_key,
            "distinct_id": distinct_id,
            "event": event,
            "properties": properties
        });
        let body = serde_json::to_string(&json)?;
        trace!("Sending {:?}", body);
        let res = self
            .client
            .post(format!("{}/capture", self.host))
            .body(body)
            .header("Content-Type", "application/json")
            .send()
            .await?;
        Ok(res)
    }

    /// Directly request a user & guild combo feature flags from Posthog
    async fn request_feature_flags(
        &self,
        distinct_id: &str,
        guild_id: Option<impl Into<u64>>,
    ) -> Result<HashMap<String, String>, anyhow::Error> {
        let mut groups = json!({});

        if let Some(guild_id) = guild_id {
            groups["guild"] = Value::String(guild_id.into().to_string());
        }

        let json = json!({
            "api_key": &*self.api_key,
            "distinct_id": distinct_id,
            "groups": groups
        });
        let body = serde_json::to_string(&json)?;
        trace!("{:?}", body);
        let res = self
            .client
            .post(format!("{}/decide?v=3", self.host))
            .body(body)
            .header("Content-Type", "application/json")
            .send()
            .await?;

        let res = res.json::<Value>().await?;

        trace!("Flags response: {:?}", res);

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

        Ok(flags)
    }

    /// Get feature flags for a user in a guild, utilising a local cache
    pub async fn get_feature_flags(
        &self,
        distinct_id: &str,
        guild_id: Option<impl Into<u64> + Clone>,
    ) -> Result<HashMap<String, String>, anyhow::Error> {
        // We only have a cache for guilds not specific users
        if let Some(guild_id) = guild_id.clone() {
            let guild_id = guild_id.into();

            // Check for cache hit
            let cache = self.cache.read().await;
            if let Some(cached_flags) = cache.get(&guild_id) {
                if Instant::now().duration_since(cached_flags.last_fetched) < CACHE_DURATION {
                    return Ok(cached_flags.flags.clone());
                }
            }

            drop(cache);
            trace!("No cache hit, requesting from posthog");
            // No cache hit, so let's request from Posthog
            let flags = self
                .request_feature_flags(distinct_id, Some(guild_id))
                .await?;

            let mut cache = self.cache.write().await;
            cache.insert(
                guild_id,
                FeatureFlags {
                    flags: flags.clone(),
                    last_fetched: Instant::now(),
                },
            );

            return Ok(flags);
        }

        // No guild specified so can't use cache
        self.request_feature_flags(distinct_id, guild_id).await
    }

    pub async fn get_feature_flag<T: DeserializeOwned + Default>(
        &self,
        distinct_id: &str,
        guild_id: Option<impl Into<u64> + Clone>,
        flag: &str,
    ) -> Result<T, anyhow::Error> {
        let flags = self.get_feature_flags(distinct_id, guild_id).await?;

        match flags.get(flag) {
            Some(x) => Ok(serde_json::from_str(x)?),
            None => Ok(T::default()),
        }
    }
}
