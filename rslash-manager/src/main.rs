use std::env;
use std::process::Command as Cmd;
use serde_json;
use redis;
use redis::Commands;
use std::sync::Arc;
use kube::api::Patch;
use futures_util::TryStreamExt;
use serenity::client::{Context, EventHandler};
use serenity::prelude::{GatewayIntents, TypeMapKey};
use serenity::async_trait;
use serenity::model::application::command::{Command, CommandOptionType};
use serenity::model::gateway::Ready;

use mongodb::{options::ClientOptions};
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use serenity::cache::Cache;
use serenity::http::GuildPagination;
use serenity::model::guild::GuildInfo;
use serenity::model::id::GuildId;
use serenity::model::Timestamp;


async fn get_redis_connection() -> redis::Connection {
    let mut get_ip = Cmd::new("kubectl");
    get_ip.arg("get").arg("nodes").arg("-o").arg("json");
    let output = get_ip.output().expect("Failed to run kubectl");
    let ip_output = String::from_utf8(output.stdout).expect("Failed to convert output to string");
    let ip_json: Result<serde_json::Value, serde_json::Error> = serde_json::from_str(&ip_output);
    let ip = ip_json.expect("Failed to convert string to json")["items"][0]["status"]["addresses"][0]["address"].to_string().replace('"', "");
    let db_client = redis::Client::open(format!("redis://{}:31090/", ip)).unwrap();
    let con = db_client.get_connection().expect("Can't connect to redis");
    return con;
}


async fn update_max_unavailable() {
    let (_, max_concurrency) = get_discord_details().await;
    let client_k8s = kube::Client::try_default().await.unwrap();

    let namespace = env::var("NAMESPACE").expect("NAMESPACE not set");

    let stateful_sets: kube::Api<k8s_openapi::api::apps::v1::StatefulSet> = kube::Api::namespaced(client_k8s, &namespace);
    let patch = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": {
            "name": "discord-shards",
            "namespace": namespace
        },
        "spec": {
            "updateStrategy": {
                "type": "RollingUpdate",
                "rollingUpdate": {
                    "maxUnavailable": max_concurrency
                }
            }
        }
    });

    let mut params = kube::api::PatchParams::apply("rslash-manager");
    params.force = true;
    let patch = Patch::Apply(&patch);
    let _ = stateful_sets.patch("discord-shards", &params, &patch).await.expect("Failed to patch statefulset discord-shards");
}

async fn get_discord_details() -> (u64, u64) {
    let namespace = env::var("NAMESPACE").expect("NAMESPACE not set");

    let client_k8s = kube::Client::try_default().await.unwrap();
    let credentials: kube::Api<k8s_openapi::api::core::v1::Secret> = kube::Api::namespaced(client_k8s, &namespace);
    let credentials = credentials.get("discord-credentials").await.unwrap();
    let credentials = credentials.data.unwrap();
    let token = credentials["DISCORD_TOKEN"].clone();
    let token = String::from_utf8(token.0).unwrap();

    let web_client = reqwest::Client::new();
    let res = web_client
            .get("https://discord.com/api/v8/gateway/bot")
            .header("Authorization", format!("Bot {}", token))
            .send()
            .await;

    let json: serde_json::Value = res.unwrap().json().await.unwrap();
    let total_shards: u64 = serde_json::from_value::<u64>(json["shards"].clone()).expect("Gateway response not u64");
    let max_concurrency: u64 = serde_json::from_value::<u64>(json["session_start_limit"]["max_concurrency"].clone()).expect("Gateway response not u64");

    return (total_shards, max_concurrency);
}


async fn stop() -> Result<(), Box<dyn std::error::Error>> {
    let mut con = get_redis_connection().await;
    let _:() = con.set("manual_sharding", "true")?; // Tells discord-interface to release control of sharding

    k8s_interface::scale_to(Some(0), Some(env::var("NAMESPACE")?)).await?;
    Ok(())
}


async fn start() -> Result<(), Box<dyn std::error::Error>> {
    let mut con = get_redis_connection().await;
    let _:() = con.set("manual_sharding", "true")?; // Tells discord-interface to release control of sharding

    k8s_interface::scale_to(None, Some(env::var("NAMESPACE")?)).await?;
    let _:() = con.set("manual_sharding", "false")?; // Tells discord-interface to take control of sharding again
    Ok(())
}


/// Fetches a guild's configuration, creating it if it doesn't exist.
async fn fetch_guild_config(guild_id: GuildId, cache: Arc<Cache>, mongodb_client: &mut mongodb::Client) -> Document {
    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("servers");

    let filter = doc! {"id": guild_id.0.to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    return match cursor.try_next().await.unwrap() {
        Some(doc) => doc, // If guild configuration does exist
        None => { // If guild configuration doesn't exist
            let joined_at = if let Some(i) = guild_id.to_guild_cached(&cache) {
                i.joined_at
            } else {
                Timestamp::from_unix_timestamp(0).unwrap()
            };
            // If the bot joined the guild before September 1st 2022, it joined as Booty Bot, not R Slash
            let nsfw = if joined_at < Timestamp::from_unix_timestamp(1661986800).unwrap() {
                "nsfw"
            } else {
                "non-nsfw"
            };

            let server = doc! {
                "id": guild_id.0.to_string(),
                "nsfw": nsfw,
            };

            coll.insert_one(server, None).await.unwrap();
            let mut cursor = coll.find(filter, find_options).await.unwrap();
            cursor.try_next().await.unwrap().unwrap()
        }
    };
}


struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, _ready: Ready) {
        println!("Updating slash commands");
        let command = ctx.data.read().await;
        let command = command.get::<CommandToUpdate>().unwrap();

        if command == "delete" {
            let command: u64 = env::args_os().nth(4).unwrap().into_string().unwrap().parse().unwrap();
            let id = serenity::model::id::CommandId::from(command);
            serenity::model::application::command::Command::delete_global_application_command(&ctx.http, id).await.expect("Failed to delete command");
        }

        if command == "guilds" {
            let mut client_options = ClientOptions::parse("mongodb://my-user:rslash@localhost:27018/?tls=false&directConnection=true").await.unwrap();
            client_options.app_name = Some("rslash-manager".to_string());

            let mut mongodb_client = mongodb::Client::with_options(client_options).unwrap();

            let db = mongodb_client.database("config");
            let coll = db.collection::<Document>("settings");

            let filter = doc! {"id": "subreddit_list".to_string()};
            let find_options = FindOptions::builder().build();
            let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

            let doc = cursor.try_next().await.unwrap().unwrap();
            let sfw_subreddits: Vec<&str> = doc.get_array("sfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();
            let nsfw_subreddits: Vec<&str> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();

            let mut guilds = Vec::new();
            loop {
                let mut page = ctx.http.as_ref().get_guilds(Some(&GuildPagination::After(guilds.last().map_or(GuildId(1), |g: &GuildInfo| g.id))), Some(200)).await.unwrap();
                if page.len() == 0 {
                    break;
                }
                println!("Reached guild {}", guilds.len());
                guilds.append(&mut page);
            }
            println!("Number of guilds: {}", guilds.len());
            let mut count = 0;
            for guild in guilds {
                count += 1;
                if count % 100 == 0 {
                    println!("Created commands on guild {}", count);
                }
                let id = guild.id;

                let guild_config = fetch_guild_config(id, ctx.cache.clone(), &mut mongodb_client).await;
                let nsfw = guild_config.get_str("nsfw").unwrap();

                let _ = id.create_application_command(&ctx.http, |command| {
                command
                    .name("get")
                    .description("Get a post from a specified subreddit");

                command.create_option(|option| {
                    option.name("subreddit")
                        .description("The subreddit to get a post from")
                        .required(true)
                        .kind(CommandOptionType::String);

                    if nsfw == "nsfw" || nsfw == "both" {
                        for subreddit in &nsfw_subreddits {
                            option.add_string_choice(subreddit, subreddit);
                        }
                    }
                    if nsfw == "non-nsfw" || nsfw == "both" {
                        for subreddit in &sfw_subreddits {
                            option.add_string_choice(subreddit, subreddit);
                        }
                    }

                    return option;
                });

                return command;
            }).await;
            }
        }

        if command == "get_delete" {
            let mut guilds = Vec::new();
            loop {
                let mut page = ctx.http.as_ref().get_guilds(Some(&GuildPagination::After(guilds.last().map_or(GuildId(1), |g: &GuildInfo| g.id))), Some(200)).await.unwrap();
                if page.len() == 0 {
                    break;
                }
                println!("Reached guild {}", guilds.len());
                guilds.append(&mut page);
            }
            println!("Number of guilds: {}", guilds.len());
            let mut count = 0;
            for guild in guilds {
                count += 1;
                if count % 100 == 0 {
                    println!("Created commands on guild {}", count);
                }
                let id = guild.id;
                match id.set_application_commands(&ctx.http, |c| {
                    c.set_application_commands(Vec::new())
                }).await {
                    Ok(_) => {},
                    Err(e) => println!("Error: {}", e)
                }; // Delete all guild commands
            }
        }

        if command == "ping_delete" {
            let id = serenity::model::id::CommandId::from(1053326533651071067);
            Command::delete_global_application_command(&ctx.http, id).await.expect("Failed to delete command");
        }

        if command == "get" || command == "all" {
            let mut client_options = ClientOptions::parse("mongodb://my-user:rslash@localhost:27018/?tls=false&directConnection=true").await.unwrap();
            client_options.app_name = Some("rslash-manager".to_string());

            let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

            let db = mongodb_client.database("config");
            let coll = db.collection::<Document>("settings");

            let filter = doc! {"id": "subreddit_list".to_string()};
            let find_options = FindOptions::builder().build();
            let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

            let doc = cursor.try_next().await.unwrap().unwrap();
            let sfw_subreddits: Vec<&str> = doc.get_array("sfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();
            let nsfw_subreddits: Vec<&str> = doc.get_array("nsfw").unwrap().into_iter().map(|x| x.as_str().unwrap()).collect();

            let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
            let nsfw = application_id == 278550142356029441;
            let _ = Command::create_global_application_command(&ctx.http,|command| {
                command
                    .name("get")
                    .description("Get a post from a specified subreddit");

                command.create_option(|option| {
                    option.name("subreddit")
                        .description("The subreddit to get a post from")
                        .required(true)
                        .kind(CommandOptionType::String);

                    if nsfw {
                        for subreddit in &nsfw_subreddits {
                            option.add_string_choice(subreddit, subreddit);
                        }
                    }
                    else {
                        for subreddit in &sfw_subreddits {
                            option.add_string_choice(subreddit, subreddit);
                        }
                    }

                    return option;
                });

                command.create_option(|option| {
                    option.name("search")
                        .description("Search by title")
                        .required(false)
                        .kind(CommandOptionType::String)
                });

                return command;
            }).await;
        }

        if command == "membership" || command == "all" {
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("membership")
                    .description("Get information about your membership")
            }).await.expect("Failed to register slash commands");
        }

        if command == "ping" || command == "all" {
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("ping")
                    .description("Basic command to check if the bot is online")
            }).await.expect("Failed to register slash command");
        }

        if command == "support" || command == "all" {
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("support")
                    .description("Get help with the bot.")
            }).await.expect("Failed to register slash command");
        }

        if command == "info" || command == "all" {
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("info")
                    .description("Get information about the bot")
            }).await.expect("Failed to register slash commands");
        }

        if command == "custom" || command == "all" {
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("custom")
                    .description("PREMIUM: Get post from a custom subreddit")
                    .create_option(|option| {
                        option.name("subreddit")
                            .description("The subreddit to get a post from")
                            .required(true)
                            .kind(CommandOptionType::String)
                    })
                    .create_option( |option| {
                        option.name("search")
                            .description("Search by title")
                            .required(false)
                            .kind(CommandOptionType::String)
                    })
            }).await.expect("Failed to register slash commands");
        }

        if command == "configure" {
            println!("Matched on configure");
            let _ = Command::create_global_application_command(&ctx.http, |command| {
                command
                    .name("configure-server")
                    .description("Configure the bot")
                    .default_member_permissions(serenity::model::permissions::Permissions::MANAGE_GUILD);

                command.create_option(|option| {
                    option.name("commands")
                        .description("Configure server-level command behaviour")
                        .kind(CommandOptionType::SubCommandGroup)
                        .create_sub_option(|sub_option| {
                            sub_option.name("nsfw")
                                .kind(CommandOptionType::SubCommand)
                                .description("Whether to show NSFW subreddits or not")
                                .create_sub_option(|sub_option| {
                                    sub_option.name("value")
                                        .required(true)
                                        .description("You can allow members to only use SFW subreddits, NSFW subreddits, or both.")
                                        .add_string_choice("Only show NSFW subreddits", "nsfw")
                                        .add_string_choice("Only show non-NSFW subreddits", "non-nsfw")
                                        .add_string_choice("Show both NSFW and non-NSFW subreddits", "both")
                                        .kind(CommandOptionType::String)
                                })

                        })
                });
                return command;
            }).await.expect("Failed to register slash commands");
        }
        println!("Exiting");
        std::process::exit(0);
    }
}

struct CommandToUpdate;
impl TypeMapKey for CommandToUpdate {
    type Value = String;
}

async fn update_commands(command: Option<&str>) {
    let command: &str = match command {
        Some(x) => x,
        None => "all"
    };

    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID").expect("DISCORD_APPLICATION_ID not set").parse().expect("Failed to convert application_id to u64");
    let shard_id: usize = 0;
    let total_shards: usize = env::var("TOTAL_SHARDS").expect("TOTAL_SHARDS not set").parse().expect("Failed to convert total_shards to usize");

    let mut client = serenity::Client::builder(token, GatewayIntents::non_privileged())
        .event_handler(Handler)
        .application_id(application_id)
        .await
        .expect("Error creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<CommandToUpdate>(command.to_string());
    }

    client.start_shard(shard_id as u64, total_shards as u64).await.expect("Error starting shard");

}


async fn update() {
    let component = env::args_os().nth(2).unwrap().into_string().unwrap();
    let namespace = env::var("NAMESPACE").expect("NAMESPACE not set");

    match component.as_str() {
        "shards" => {
            update_max_unavailable().await;

            match {env::args_os().nth(3)} { // Check if a tag is provided
                Some(x) => {
                    let tag: String = x.into_string().unwrap().parse().unwrap();
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("set").arg("-n").arg(namespace).arg("image").arg("statefulset/discord-shards").arg(format!("discord-shard=registry.murraygrov.es/discord-shard:{}", tag));
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                },
                None => {
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("rollout").arg("-n").arg(namespace).arg("restart").arg("statefulset/discord-shards");
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                }
            };
        },

        "subreddits" => {
            let action = env::args_os().nth(3).unwrap().into_string().unwrap();
            match action.as_str() {
                "add" => {
                    update_commands(Some("get")).await;
                },
                "remove" => {

                },
                _ => {
                    println!("Invalid action");
                    return;
                }
            }
        },

        "commands" => {
            let command = env::args_os().nth(3).unwrap().into_string().unwrap();
            update_commands(Some(&command)).await;
        },

        "discord-interface" => {
            match {env::args_os().nth(3)} { // Check if a tag is provided
                Some(x) => {
                    let tag: String = x.into_string().unwrap().parse().unwrap();
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("set").arg("-n").arg(namespace).arg("image").arg("deployment/discord-interface").arg(format!("discord-interface=discord-interface:{}", tag));
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                },
                None => {
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("rollout").arg("-n").arg(namespace).arg("restart").arg("deployment/discord-interface");
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                }
            };
        },

        "downloader" => {
            match {env::args_os().nth(3)} { // Check if a tag is provided
                Some(x) => {
                    let tag: String = x.into_string().unwrap().parse().unwrap();
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("set").arg("-n").arg(namespace).arg("image").arg("deployment/reddit-downloader").arg(format!("reddit-downloader=reddit-downloader:{}", tag));
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                },
                None => {
                    let mut rollout = Cmd::new("kubectl");
                    rollout.arg("rollout").arg("-n").arg(namespace).arg("restart").arg("deployment/reddit-downloader");
                    let output = rollout.output().expect("Failed to run kubectl");
                    println!("{:?}", String::from_utf8(output.stdout).unwrap());
                    println!("{:?}", String::from_utf8(output.stderr).unwrap());
                }
            };
        }
        _ => {
            println!("No component/invalid component provided");
        }
    }

}


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>>{
    match env::args_os().nth(1).expect("No command provided").to_str().unwrap() {
        "start" => start().await?,
        "stop" => stop().await?,
        "update" => update().await,
        "restart" => update().await, // Calling update without a tag just restarts all shards anyway
        _ => println!("No Command Provided"),
    };

    Ok(())
}