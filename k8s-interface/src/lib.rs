use std::fs;
use std::process::Command;

use kube::api::Patch;
use redis::Commands;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}


// Determine namespace we are running in
pub fn get_namespace() -> Result<String, Box<dyn std::error::Error>> {
    Ok(fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")?)
}


async fn get_redis_connection() -> Result<redis::Connection, Box<dyn std::error::Error>> {
    let mut get_ip = Command::new("kubectl");
    get_ip.arg("get").arg("nodes").arg("-o").arg("json");
    let output = get_ip.output()?;
    let ip_output = String::from_utf8(output.stdout)?;
    let ip_json: Result<serde_json::Value, serde_json::Error> = serde_json::from_str(&ip_output);
    let ip = ip_json?["items"][0]["status"]["addresses"][0]["address"].to_string().replace('"', "");
    let db_client = redis::Client::open(format!("redis://{}:31090/", ip))?;
    
    Ok(db_client.get_connection()?)
}


// Returns total shards and max concurrency
async fn get_discord_connection_details(token: String) -> Result<(u64, u64), Box<dyn std::error::Error>> {
    let web_client = reqwest::Client::new();
    let res = web_client
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

    let json: serde_json::Value = res.unwrap().json().await?;
    let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone())?;
    let max_concurrency: u64 = serde_json::from_value::<u64>(json["session_start_limit"]["max_concurrency"].clone())?;

    Ok((total_shards, max_concurrency))
}


async fn get_ready_shards(namespace: Option<String>) -> Result<u64, Box<dyn std::error::Error>> {
    let namespace = namespace.unwrap_or(get_namespace()?);
    let client_k8s = kube::Client::try_default().await?;
    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
    let shards_set = stateful_sets.get("discord-shards").await?;
    let ready_shards: u64 = shards_set.status.unwrap().available_replicas.unwrap().try_into()?;

    Ok(ready_shards)
}


async fn get_desired_shards(namespace: Option<String>) -> Result<u64, Box<dyn std::error::Error>> {
    let namespace = namespace.unwrap_or(get_namespace()?);
    let client_k8s = kube::Client::try_default().await?;
    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
    let shards_set = stateful_sets.get("discord-shards").await?;
    let ready_shards: u64 = shards_set.status.unwrap().replicas.try_into()?;

    Ok(ready_shards)
}


async fn set_shards(num: u64, update_redis: bool, namespace: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let namespace = namespace.unwrap_or(get_namespace()?);

    if update_redis {
        let mut con = get_redis_connection().await?;
        con.set(format!("total_shards_{}", &namespace), num)?;
    }

    let patch = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {
            "name": "discord-shards",
            "namespace": namespace
        },
        "spec": {
            "replicas": num
        }
    });

    let client_k8s = kube::Client::try_default().await?;
    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);

    let mut params = kube::api::PatchParams::apply("k8s-interface");
    params.force = true;
    let patch = Patch::Apply(&patch);
    stateful_sets.patch("discord-shards", &params, &patch).await?;

    Ok(())
}


async fn add_shards(num: u64, max_concurrency: u64, namespace: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let namespace = namespace.unwrap_or(get_namespace()?);
    let mut con = get_redis_connection().await?;

    let mut current_shards: u64 = con.get(format!("total_shards_{}", namespace))?;
    let total_shards = current_shards + num;

    con.set(format!("total_shards_{}", namespace), total_shards)?;

    let mut shards_left = num;

    while shards_left > 0 {
        let to_add = std::cmp::min(shards_left, max_concurrency);
        shards_left -= to_add;
        current_shards += to_add;

        set_shards(current_shards, false, Some(namespace.clone())).await?;

        // Sleep until current batch is ready
        while get_ready_shards(Some(namespace.clone())).await? != current_shards {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    Ok(())
}


// Scale to total_shards, if None: find total_shards from Discord
pub async fn scale_to(total_shards: Option<u64>, namespace: Option<String>) -> Result<(), Box<dyn std::error::Error>>{
    let namespace = namespace.unwrap_or(get_namespace()?);

    let token = std::env::var("DISCORD_TOKEN")?;
    let (total_shards_api, max_concurrency) = get_discord_connection_details(token).await?;

    let total_shards = match total_shards {
        Some(x) => x,
        None => total_shards_api
    };

    let current_shards = get_desired_shards(Some(namespace.clone())).await?;
    if total_shards > current_shards {
        add_shards(total_shards - current_shards, max_concurrency, Some(namespace.clone())).await?;
    } else {
        set_shards(total_shards, true, Some(namespace.clone())).await?;
    }
    Ok(())
}