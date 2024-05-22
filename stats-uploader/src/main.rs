use std::{collections::HashMap, time::Duration};

use redis::{Commands, from_redis_value};
use tokio::time::sleep;

#[tokio::main]
async fn main() {
    let discord_bots_gg_token = std::env::var("DISCORD_BOTS_GG_TOKEN").expect("No DISCORD_BOTS_GG_TOKEN env variable set");
    let top_gg_token = std::env::var("TOP_GG_TOKEN").expect("No TOP_GG_TOKEN env variable set");
    let discordlist_gg_token = std::env::var("DISCORDLIST_GG_TOKEN").expect("No DISCORDLIST_GG_TOKEN env variable set");
    let discords_com_token = std::env::var("DISCORDS_COM_TOKEN").expect("No DISCORDS_COM_TOKEN env variable set");

    let client = reqwest::Client::new();
    let mut con = redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").expect("Failed to open redis client");

    let namespaces = HashMap::from([
        ("booty-bot", "278550142356029441"),
        ("r-slash", "282921751141285888")
    ]);


    loop {
        for (namespace, id) in &namespaces {
            let guild_counts: HashMap<String, redis::Value> = con.hgetall(format!("shard_guild_counts_{}", namespace)).unwrap();
            let mut guild_count = 0;
            for (_, count) in guild_counts {
                guild_count += from_redis_value::<u64>(&count).unwrap();
            }

            let url = format!("https://discord.bots.gg/api/v1/bots/{}/stats", id);
            let body = format!("{{\"guildCount\": {}}}", guild_count);

            let res = client.post(&url)
                .header("Authorization", discord_bots_gg_token.clone())
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await;

            match res {
                Ok(_) => println!("Updated {}'s stats on bots.gg", namespace),
                Err(e) => println!("Error updating {}'s stats on bots.gg: {}", namespace, e)
            }

            if id == &"282921751141285888" {
                let url = format!("https://top.gg/api/bots/{}/stats", id);
                let body = format!("{{\"server_count\": {}}}", guild_count);

                let res = client.post(&url)
                    .header("Authorization", top_gg_token.clone())
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;

                match res {
                    Ok(_) => println!("Updated {}'s stats on top.gg", namespace),
                    Err(e) => println!("Error updating {}'s stats on top.gg: {}", namespace, e)
                }
            } else {
                let url = format!("https://discordlist.gg/bots/{}/stats", id);
                let body = format!("{{\"server_count\": {}}}", guild_count);

                let res = client.post(&url)
                    .header("Authorization", discordlist_gg_token.clone())
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;

                match res {
                    Ok(_) => println!("Updated {}'s stats on discordlist.gg", namespace),
                    Err(e) => println!("Error updating {}'s stats on discordlist.gg: {}", namespace, e)
                }

                let url = format!("https://discords.com/bots/api/{}/setservers", id);
                let body = format!("{{\"server_count\": {}}}", guild_count);

                let res = client.post(&url)
                    .header("Authorization", discords_com_token.clone())
                    .header("Content-Type", "application/json")
                    .body(body)
                    .send()
                    .await;

                match res {
                    Ok(_) => println!("Updated {}'s stats on discords.com", namespace),
                    Err(e) => println!("Error updating {}'s stats on discords.com: {}", namespace, e)
                }
            }

            sleep(Duration::from_secs(60)).await;
        }
    }
}
