use futures_util::TryStreamExt;
use redis;
use redis::Commands;
use serde_json;
use serenity::all::{CreateCommand, CreateCommandOption};
use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::application::{Command, CommandOptionType};
use serenity::model::gateway::Ready;
use serenity::prelude::{GatewayIntents, TypeMapKey};
use std::env;
use std::process::Command as Cmd;
use std::sync::Arc;

use mongodb::bson::{Document, doc};
use mongodb::options::ClientOptions;
use mongodb::options::FindOptions;
use serenity::cache::Cache;
use serenity::http::GuildPagination;
use serenity::model::Timestamp;
use serenity::model::guild::GuildInfo;
use serenity::model::id::GuildId;

async fn get_redis_connection() -> redis::Connection {
    let db_client = redis::Client::open("redis://100.96.244.19:6379/").unwrap();
    let con = db_client.get_connection().expect("Can't connect to redis");
    return con;
}

/// Fetches a guild's configuration, creating it if it doesn't exist.
async fn fetch_guild_config(
    guild_id: GuildId,
    cache: Arc<Cache>,
    mongodb_client: &mut mongodb::Client,
) -> Document {
    let db = mongodb_client.database("config");
    let coll = db.collection::<Document>("servers");

    let filter = doc! {"id": guild_id.get().to_string()};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll
        .find(filter.clone(), find_options.clone())
        .await
        .unwrap();

    return match cursor.try_next().await.unwrap() {
        Some(doc) => doc, // If guild configuration does exist
        None => {
            // If guild configuration doesn't exist
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
                "id": guild_id.get().to_string(),
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

        let mut client_options = ClientOptions::parse(
            "mongodb://r-slash:r-slash@100.97.36.37:27017/?tls=false&directConnection=true",
        )
        .await
        .unwrap();
        client_options.app_name = Some("rslash-manager".to_string());

        let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

        let db = mongodb_client.database("config");
        let coll = db.collection::<Document>("settings");

        let filter = doc! {"id": "subreddit_list".to_string()};
        let find_options = FindOptions::builder().build();
        let mut cursor = coll
            .find(filter.clone(), find_options.clone())
            .await
            .unwrap();

        let doc = cursor.try_next().await.unwrap().unwrap();
        let sfw_subreddits: Vec<&str> = doc
            .get_array("sfw")
            .unwrap()
            .into_iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        let nsfw_subreddits: Vec<&str> = doc
            .get_array("nsfw")
            .unwrap()
            .into_iter()
            .map(|x| x.as_str().unwrap())
            .collect();

        let application_id: u64 = env::var("DISCORD_APPLICATION_ID")
            .expect("DISCORD_APPLICATION_ID not set")
            .parse()
            .expect("Failed to convert application_id to u64");
        let nsfw = application_id == 278550142356029441;

        let mut subreddit_options = CreateCommandOption::new(
            CommandOptionType::String,
            "subreddit",
            "The subreddit to subscribe to",
        )
        .required(true);

        if nsfw {
            for subreddit in &nsfw_subreddits {
                subreddit_options = subreddit_options
                    .add_string_choice(subreddit.to_string(), subreddit.to_string());
            }
        } else {
            for subreddit in &sfw_subreddits {
                subreddit_options = subreddit_options
                    .add_string_choice(subreddit.to_string(), subreddit.to_string());
            }
        }

        if command == "delete" {
            let command: u64 = env::args_os()
                .nth(4)
                .unwrap()
                .into_string()
                .unwrap()
                .parse()
                .unwrap();
            let id = serenity::model::id::CommandId::from(command);
            serenity::model::application::Command::delete_global_command(&ctx.http, id)
                .await
                .expect("Failed to delete command");
        }

        if command == "ping_delete" {
            let id = serenity::model::id::CommandId::from(1053326533651071067);
            Command::delete_global_command(&ctx.http, id)
                .await
                .expect("Failed to delete command");
        }

        if command == "get" || command == "all" {
            let builder =
                CreateCommand::new("get").description("Get a post from a specified subreddit");

            let search =
                CreateCommandOption::new(CommandOptionType::String, "search", "Search by title");

            let _ = Command::create_global_command(
                &ctx.http,
                builder
                    .add_option(subreddit_options.clone())
                    .add_option(search),
            )
            .await;
        }

        if command == "subscribe" || command == "all" {
            let builder = CreateCommand::new("subscribe")
                .description("Subscribe to new posts from a specified subreddit");

            let _ = Command::create_global_command(
                &ctx.http,
                builder.add_option(subreddit_options.clone()),
            )
            .await;
        }

        if command == "unsubscribe" || command == "all" {
            let _ = Command::create_global_command(&ctx.http,
                                                   CreateCommand::new("unsubscribe")
                                                       .description("Unsubscribe from a subreddit, using this command will allow you to select which one"),
            ).await.expect("Failed to register slash commands");
        }

        if command == "membership" || command == "all" {
            let _ = Command::create_global_command(
                &ctx.http,
                CreateCommand::new("membership")
                    .description("Get information about your membership"),
            )
            .await
            .expect("Failed to register slash commands");
        }

        if command == "support" || command == "all" {
            let _ = Command::create_global_command(
                &ctx.http,
                CreateCommand::new("support").description("Get help with the bot."),
            )
            .await
            .expect("Failed to register slash command");
        }

        if command == "info" || command == "all" {
            Command::create_global_command(
                &ctx.http,
                CreateCommand::new("info").description("Get information about the bot"),
            )
            .await
            .expect("Failed to register slash commands");
        }

        if command == "custom" || command == "all" {
            let _ = Command::create_global_command(
                &ctx.http,
                CreateCommand::new("custom")
                    .description("PREMIUM: Get post from a custom subreddit")
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "subreddit",
                            "The subreddit to get a post from",
                        )
                        .required(true)
                        .set_autocomplete(true),
                    )
                    .add_option(CreateCommandOption::new(
                        CommandOptionType::String,
                        "search",
                        "Search by title",
                    )),
            )
            .await
            .expect("Failed to register slash commands");
        }

        if command == "subscribe_custom" || command == "all" {
            let _ = Command::create_global_command(
                &ctx.http,
                CreateCommand::new("subscribe_custom")
                    .description("PREMIUM: Subscribe to posts from a custom subreddit")
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "subreddit",
                            "The subreddit to subscribe to",
                        )
                        .required(true)
                        .set_autocomplete(true),
                    ),
            )
            .await
            .expect("Failed to register slash commands");
        }

        if command == "autopost" || command == "all" {
            Command::create_global_command(
                &ctx.http,
                CreateCommand::new("autopost")
                    .description("Manage Autoposts for this channel")
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::SubCommand,
                            "start",
                            "Setup an autopost for a specified subreddit in this channel",
                        )
                        .add_sub_option(subreddit_options.clone())
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::String,
                            "search",
                            "The search to use when getting posts",
                        )),
                    )
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::SubCommand,
                            "custom",
                            "Setup an autopost for a custom subreddit in this channel",
                        )
                        .add_sub_option(
                            CreateCommandOption::new(
                                CommandOptionType::String,
                                "subreddit",
                                "The search to use when getting posts",
                            )
                            .required(true),
                        )
                        .add_sub_option(CreateCommandOption::new(
                            CommandOptionType::String,
                            "search",
                            "The search to use when getting posts",
                        )),
                    )
                    .add_option(CreateCommandOption::new(
                        CommandOptionType::SubCommand,
                        "stop",
                        "Lets you choose an autopost in this channel to stop",
                    )),
            )
            .await
            .expect("Failed to register slash commands");
        }

        if command == "config" || command == "all" {
            Command::create_global_command(
                &ctx.http,
                CreateCommand::new("config")
                    .description("Configure the bot in this server")
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::SubCommand,
                            "starboard",
                            "Channel to send messages with a star reaction to",
                        )
                        .add_sub_option(
                            CreateCommandOption::new(
                                CommandOptionType::Channel,
                                "channel",
                                "Target channel",
                            )
                            .required(true),
                        ),
                    ),
            )
            .await
            .expect("Failed to register slash command");
        };
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
        None => "all",
    };

    let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID")
        .expect("DISCORD_APPLICATION_ID not set")
        .parse()
        .expect("Failed to convert application_id to u64");
    let shard_id: usize = 0;
    let total_shards: usize = env::var("TOTAL_SHARDS")
        .expect("TOTAL_SHARDS not set")
        .parse()
        .expect("Failed to convert total_shards to usize");

    let mut client = serenity::Client::builder(token, GatewayIntents::non_privileged())
        .event_handler(Handler)
        .application_id(application_id.into())
        .await
        .expect("Error creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<CommandToUpdate>(command.to_string());
    }

    client
        .start_shard(shard_id as u32, total_shards as u32)
        .await
        .expect("Error starting shard");
}

async fn update() {
    let component = env::args_os().nth(2).unwrap().into_string().unwrap();

    match component.as_str() {
        "subreddits" => {
            let action = env::args_os().nth(3).unwrap().into_string().unwrap();
            match action.as_str() {
                "add" => {
                    update_commands(Some("get")).await;
                }
                "remove" => {}
                _ => {
                    println!("Invalid action");
                    return;
                }
            }
        }

        "commands" => {
            let command = env::args_os().nth(3).unwrap().into_string().unwrap();
            update_commands(Some(&command)).await;
        }

        _ => {
            println!("No component/invalid component provided");
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match env::args_os()
        .nth(1)
        .expect("No command provided")
        .to_str()
        .unwrap()
    {
        "update" => update().await,
        "restart" => update().await, // Calling update without a tag just restarts all shards anyway
        _ => println!("No Command Provided"),
    };

    Ok(())
}
