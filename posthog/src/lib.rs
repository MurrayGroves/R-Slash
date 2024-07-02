use log::*;
use serde_json::json;
use serde_json::Value;

#[derive(Clone)]
pub struct Client {
    pub api_key: String,
    pub host: String,
    pub client: reqwest::Client,
}
    
fn merge(a: &mut Value, b: Value) {
    match (a, b) {
        (a @ &mut Value::Object(_), Value::Object(b)) => {
            let a = a.as_object_mut().unwrap();
            for (k, v) in b {
                merge(a.entry(k).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

impl Client {
    pub fn new(api_key: String, host: String) -> Client {
        Client {
            api_key,
            host,
            client: reqwest::Client::new(),
        }
    }

    pub async fn capture(&self, event: &str,  properties: impl Into<serde_json::Value>, distinct_id: &str) -> Result<reqwest::Response, reqwest::Error> {
        let mut properties: serde_json::Value = properties.into();
        let properties_2 = json!({
            "distinct_id": distinct_id
        });
        merge(&mut properties, properties_2);
        let json = json!({
            "api_key": self.api_key,
            "event": event,
            "properties": properties
        });
        let body = serde_json::to_string(&json).unwrap();
        trace!("{:?}", body);
        let res = self.client.post(&self.host).body(body).header("Content-Type", "application/json").send().await?;
        Ok(res)
    }
}