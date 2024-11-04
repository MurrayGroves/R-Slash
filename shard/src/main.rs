#![deny(elided_lifetimes_in_paths)]

use futures::FutureExt;
use log::trace;
use serde_json::{json, Value};
use serenity::all::{AutocompleteChoice, ChannelId, CreateActionRow, CreateAutocompleteResponse, CreateButton, CreateCommand, Emoji, GuildId, ReactionType};
use serenity::gateway::ShardStageUpdateEvent;
use serenity::model::Colour;
use stubborn_io::tokio::{StubbornIo, UnderlyingIo};
use stubborn_io::{ReconnectOptions, StubbornTcpStream};
use tarpc::serde_transport::Transport;
use tarpc::tokio_serde::formats::Bincode;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::instrument;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{
    prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, Layer,
};

use rslash_types::{InteractionResponseMessage, ResponseFallbackMethod};
use std::collections::HashMap;
use std::env;
use std::fmt::Debug;
use std::io::Write;
use std::pin::Pin;
use std::task::Poll;
use std::{fs, iter};

use serenity::builder::{CreateEmbed, CreateInteractionResponse, CreateInteractionResponseMessage};
use serenity::model::gateway::GatewayIntents;
use serenity::model::id::ShardId;
use serenity::{async_trait, model::gateway::Ready, prelude::*};

use futures_util::TryStreamExt;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{sleep, Duration};

use redis::AsyncCommands;
use redis::{self, FromRedisValue};

use mongodb::bson::{doc, Document};
use mongodb::options::ClientOptions;
use mongodb::options::FindOptions;

use serenity::model::application::{CommandInteraction, Interaction};
use serenity::model::guild::{Guild, UnavailableGuild};

use anyhow::anyhow;
use sentry_anyhow::capture_anyhow;

mod command_handlers;
mod component_handlers;
mod discord;
mod modal_handlers;
mod feature_flags;

use rslash_types::ConfigStruct;
use rslash_types::InteractionResponse;

/// Returns current milliseconds since the Epoch
pub fn get_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

lazy_static::lazy_static! {
    static ref NAMESPACE: String = fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
    .expect("Couldn't read /var/run/secrets/kubernetes.io/serviceaccount/namespace");
}

pub fn redis_sanitise(input: &str) -> String {
    let special = vec![
        ",", ".", "<", ">", "{", "}", "[", "]", "\"", "'", ":", ";", "!", "@", "#", "$", "%", "^",
        "&", "*", "(", ")", "-", "+", "=", "~", "/"
    ];

    let mut output = input.to_string();
    for s in special {
        output = output.replace(s, &("\\".to_owned() + s));
    }

    output = output.trim().to_string();

    output
}

#[instrument(skip(data))]
pub async fn capture_event<T, U>(
    data: Arc<RwLock<TypeMap>>,
    event: &str,
    properties: Option<HashMap<&str, String>>,
    guild_id: Option<T>,
    channel_id: Option<U>,
    distinct_id: &str,
) where T: Into<u64> + Debug + Send + Clone + 'static,
        U: Into<u64> + Debug + Send + Clone + 'static
{
    let event = event.to_string();
    let properties = match properties {
        Some(properties) => Some(
            properties
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect::<HashMap<String, String>>(),
        ),
        None => None,
    };
    let distinct_id = distinct_id.to_string();

    tokio::spawn(async move {
        debug!("Getting posthog client");
        let config = data
            .read()
            .await
            .get::<ConfigStruct>()
            .unwrap()
            .clone();
        let client = config.posthog;

        let mut properties_map = serde_json::Map::new();
        if properties.is_some() {
            for (key, value) in properties.unwrap() {
                properties_map.insert(key.to_string(), serde_json::Value::String(value));
            }
        }

        match client.capture(&event, properties_map, guild_id.clone(), channel_id.clone(), &distinct_id).await {
            Ok(_) => {}
            Err(e) => {
                warn!("Error capturing event: {:?}", e);
            }
        }
    });
}

#[instrument(skip(command, ctx, tracker))]
async fn get_command_response<'a>(
    command: &'a CommandInteraction,
    ctx: &'a Context,
    mut tracker: discord::ResponseTracker<'a>,
) -> Result<(), anyhow::Error> {
    match command.data.name.as_str() {
        "ping" => {
            tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                    content: None,
                    embed: Some(
                        CreateEmbed::default()
                            .title("Pong!")
                            .color(Colour::from_rgb(0, 255, 0))
                            .to_owned(),
                    ),
                    components: None,
                    file: None,
                    ephemeral: false,
                    fallback: ResponseFallbackMethod::Error,
                }))
                .await
        }

        "support" => {
            tracker
                .send_response(InteractionResponse::Message(InteractionResponseMessage {
                    content: None,
                    embed: Some(
                        CreateEmbed::default()
                            .title("Get Support")
                            .description(
                                "[Discord Server](https://discord.gg/jYtCFQG)
                    Email: rslashdiscord@gmail.com",
                            )
                            .color(Colour::from_rgb(0, 255, 0))
                            .url("https://discord.gg/jYtCFQG")
                            .to_owned(),
                    ),
                    components: None,
                    file: None,
                    ephemeral: false,
                    fallback: ResponseFallbackMethod::Error,
                }))
                .await
        }

        "get" => command_handlers::get_subreddit_cmd(&command, &ctx, tracker).await,

        "membership" => command_handlers::cmd_get_user_tiers(&command, &ctx, tracker).await,

        "custom" => command_handlers::get_custom_subreddit(&command, &ctx, tracker).await,

        "info" => command_handlers::info(&command, &ctx, tracker).await,

        "subscribe" => command_handlers::subscribe(&command, &ctx, tracker).await,

        "subscribe_custom" => command_handlers::subscribe_custom(&command, &ctx, tracker).await,

        "unsubscribe" => command_handlers::unsubscribe(&command, &ctx, tracker).await,

        "autopost" => command_handlers::autopost(&command, &ctx, tracker).await,

        "help" => {
            capture_event(
                ctx.data.clone(),
                "cmd_help",
                None,
                command.guild_id,
                Some(command.channel_id),
                &format!("user_{}", &command.user.id.get().to_string()),
            )
            .await;
            tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
                content: Some("# Get a new post from a subreddit\n \
                Start typing `/get` in the message box. You can then either click the selection labelled `/get` or press enter.\n \
                You'll then see a list of subreddits. Click the one you want! You can then either set a search query by selecting the search option, or just send the message to get the post!\n \
                If you want to get another post from that subreddit, just click the :repeat: button underneath it!\n\
                # Make the bot send a new post again and again\n\
                Use the command `/autopost start` and select the subreddit you want to get posts from. Send that message.\n\
                You should now see a popup asking for a `Delay`, that's how long the bot will wait between each post it sends.\n\
                That popup also asks for how many times to post before stopping. For example if you say `10`, it'll send 10 posts and then you'll need to setup a new autopost.\n\
                # Make the bot send every new post that's made in a subreddit\n\
                Use the `/subscribe` command with the subreddit you want the bot to post from. Whenever someone makes a new post on that subreddit, it'll automatically post to the channel you used the command in.\n\
                # How do I use different subreddits that aren't in the list?\n\
                For $1.30 a month you can use all of the bot's commands with *any* subreddit you want! Click [here](https://ko-fi.com/rslash) to find out more and purchase.".to_string()),
                ..Default::default()
            })).await

        }

        _ => Err(anyhow!("Unknown command"))?,
    }
}

/// Discord event handler
struct Handler;

#[async_trait]
impl EventHandler for Handler {
    /// Fires when the client receives new data about a guild
    async fn guild_create(&self, ctx: Context, guild: Guild, is_new: Option<bool>) {
        debug!("Guild create event fired");
        let data_read = ctx.data.read().await;
        let conf = data_read.get::<ConfigStruct>().unwrap();
        let mut con = conf.redis.clone();
        let _: () = con
            .hset(
                format!("shard_guild_counts_{}", &*NAMESPACE),
                ctx.shard_id.0,
                ctx.cache.guild_count(),
            )
            .await
            .unwrap();

        if let Some(x) = is_new {
            // First time client has seen the guild
            if x {
                let config = ctx.data
                    .read()
                    .await
                    .get::<ConfigStruct>()
                    .unwrap()
                    .clone();

                let client = config.posthog;

                let payload = json!({
                    "api_key": client.api_key,
                    "event": "$groupidentify",
                    "properties": {
                        "distinct_id": guild.id.get().to_string(),
                        "$group_type": "guild",
                        "$group_key": guild.id.get().to_string(),
                        "$group_set": {
                            "name": guild.name,
                            "member_count_at_join": guild.member_count,
                            "owner_id": guild.owner_id.get().to_string(),
                            "joined_at": guild.joined_at.to_rfc3339(),
                        },
                    }
                });

                let body = serde_json::to_string(&payload).unwrap();
                debug!("{:?}", body);
                if let Err(e) = client
                    .client
                    .post(format!("{}/capture", client.host))
                    .body(body)
                    .header("Content-Type", "application/json")
                    .send()
                    .await {
                        error!("Error sending group identity: {:?}", e);
                };

                capture_event(
                    ctx.data.clone(),
                    "guild_join",
                    None,
                    Some(guild.id),
                    None::<ChannelId>,
                    &format!("guild_{}", guild.id.get()),
                )
                .await;

                let posthog = conf.posthog.clone();
                let feature_flags = match posthog.get_feature_flags(&format!("guild_{}", guild.id.get()), Some(guild.id)).await {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Error getting feature flags: {:?}", e);
                        return;
                    }
                };

                if let Some(variant) = feature_flags.get("help_command") {
                    if variant == "enabled" {
                        let _ = guild
                        .create_command(&ctx.http,  
                            CreateCommand::new("help")
                                .description("Learn how to use the bot")
                        )
                        .await;
                    }
                };
            }
        }
    }

    async fn guild_delete(&self, ctx: Context, incomplete: UnavailableGuild, _full: Option<Guild>) {
        {
            let data_read = ctx.data.read().await;
            let conf = data_read.get::<ConfigStruct>().unwrap();
            let mut con = conf.redis.clone();
            let _: () = con
                .hset(
                    format!("shard_guild_counts_{}", &*NAMESPACE),
                    ctx.shard_id.0,
                    ctx.cache.guild_count(),
                )
                .await
                .unwrap();
        }

        capture_event(
            ctx.data.clone(),
            "guild_leave",
            None,
            Some(incomplete.id),
            None::<ChannelId>,
            &format!("guild_{}", incomplete.id.get().to_string()),
        )
        .await;
    }

    /// Fires when the client is connected to the gateway
    async fn ready(&self, ctx: Context, ready: Ready) {
        capture_event(
            ctx.data,
            "on_ready",
            None,
            None::<GuildId>,
            None::<ChannelId>,
            &format!("shard_{}", ready.shard.unwrap().id.0.to_string()),
        )
        .await;

        info!(
            "Shard {} connected as {}, on {} servers!",
            ready.shard.unwrap().id.0,
            ready.user.name,
            ready.guilds.len()
        );

        if !Path::new("/etc/probes").is_dir() {
            match fs::create_dir("/etc/probes") {
                Ok(_) => {}
                Err(e) => {
                    if !format!("{}", e).contains("File exists") {
                        error!("Error creating /etc/probes: {:?}", e);
                    }
                }
            }
        }
        if !Path::new("/etc/probes/live").exists() {
            let mut file =
                File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
            file.write_all(b"alive")
                .expect("Unable to write to /etc/probes/live");
        }
    }

    /// Fires when the shard's status is updated
    async fn shard_stage_update(&self, _: Context, event: ShardStageUpdateEvent) {
        debug!("Shard stage changed to {:?}", event);
        let alive = match event.new {
            serenity::gateway::ConnectionStage::Connected => true,
            _ => false,
        };

        if alive {
            if !Path::new("/etc/probes/live").exists() {
                fs::create_dir_all("/etc/probes").expect("Couldn't create /etc/probes directory");
                let mut file =
                    File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
                file.write_all(b"alive")
                    .expect("Unable to write to /etc/probes/live");
            }
        } else {
            fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
        }
    }

    /// Fires when a slash command or other interaction is received
    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        debug!("Interaction received");
        let tx_ctx = sentry::TransactionContext::new(
            "interaction_create",
            "http.server",
        );
        let transaction = sentry::start_transaction(tx_ctx);
        sentry::configure_scope(|scope| scope.set_span(Some(transaction.clone().into())));

        let mut tracker = discord::ResponseTracker::new(&interaction, ctx.http.clone());

        let result = match &interaction {
            Interaction::Command(command) => {
                match command.guild_id {
                    Some(guild_id) => {
                        info!(
                            "{:?} ({:?}) > {:?} ({:?}) : /{} {:?}",
                            guild_id
                                .name(&ctx.cache)
                                .unwrap_or("Name Unavailable".into()),
                            guild_id.get(),
                            command.user.name,
                            command.user.id.get(),
                            command.data.name,
                            command.data.options
                        );
                        info!("Sent at {:?}", command.id.created_at());
                    }
                    None => {
                        info!(
                            "{:?} ({:?}) : /{} {:?}",
                            command.user.name,
                            command.user.id.get(),
                            command.data.name,
                            command.data.options
                        );
                        info!("Sent at {:?}", command.id.created_at());
                    }
                }
                get_command_response(&command, &ctx, tracker).await
            }

            Interaction::Component(command) => {
                match command.guild_id {
                    Some(guild_id) => {
                        info!(
                            "{:?} ({:?}) > {:?} ({:?}) : Button {} {:?}",
                            guild_id
                                .name(&ctx.cache)
                                .unwrap_or("Name Unavailable".into()),
                            guild_id.get(),
                            command.user.name,
                            command.user.id.get(),
                            command.data.custom_id,
                            command.data.kind
                        );
                        info!("Sent at {:?}", command.id.created_at());
                    }
                    None => {
                        info!(
                            "{:?} ({:?}) : Button {} {:?}",
                            command.user.name,
                            command.user.id.get(),
                            command.data.custom_id,
                            command.data.kind
                        );
                        info!("Sent at {:?}", command.id.created_at());
                    }
                }

                // If custom_id uses invalid data structure (old version of bot), ignore interaction
                let custom_id: HashMap<String, serde_json::Value> =
                    if let Ok(custom_id) = serde_json::from_str(&command.data.custom_id) {
                        custom_id
                    } else {
                        return;
                    };

                let button_command = match custom_id.get("command") {
                    Some(command) => command.to_string().replace('"', ""),
                    None => "again".to_string(),
                };

                match button_command.as_str() {
                        "again" => component_handlers::post_again(&ctx, &command, custom_id, tracker).await,
                        "unsubscribe" => component_handlers::unsubscribe(&ctx, &command, tracker).await,
                        "autopost_cancel" => component_handlers::autopost_cancel(&ctx, &command, tracker).await,
                        "where-autopost" => tracker.send_response(InteractionResponse::Message(InteractionResponseMessage {
                            content: Some("Auto-post setup has been moved to the `/autopost start` command to reduce clutter on the post view\nand because only users with Manage Channels can setup auto-posts so it doesn't make much sense to show to everyone.".to_string()),
                            ephemeral: true,
                            ..Default::default()
                        })).await,

                        _ => {
                            warn!("Unknown button command: {}", button_command);
                            return;
                        }
                    }
            }

            Interaction::Modal(modal) => {
                match modal.guild_id {
                    Some(guild_id) => {
                        info!(
                            "{:?} ({:?}) > {:?} ({:?}) : Modal {} {:?}",
                            guild_id
                                .name(&ctx.cache)
                                .unwrap_or("Name Unavailable".into()),
                            guild_id.get(),
                            modal.user.name,
                            modal.user.id.get(),
                            modal.data.custom_id,
                            modal.data.components
                        );
                        info!("Sent at {:?}", modal.id.created_at());
                    }
                    None => {
                        info!(
                            "{:?} ({:?}) : Modal {} {:?}",
                            modal.user.name,
                            modal.user.id.get(),
                            modal.data.custom_id,
                            modal.data.components
                        );
                        info!("Sent at {:?}", modal.id.created_at());
                    }
                }

                // If custom_id uses invalid data structure (old version of bot), ignore interaction
                let custom_id: HashMap<String, serde_json::Value> =
                    if let Ok(custom_id) = serde_json::from_str(&modal.data.custom_id) {
                        custom_id
                    } else {
                        return;
                    };

                let modal_command = match custom_id.get("command") {
                    Some(command) => command.as_str().unwrap(),
                    None => {
                        warn!("Unknown modal command: {:?}", custom_id);
                        return;
                    }
                };

                match modal_command {
                    "autopost" => {
                        modal_handlers::autopost_create(&ctx, modal, custom_id, tracker).await
                    }

                    _ => Err(anyhow!("Unknown modal command")),
                }
            }

            Interaction::Autocomplete(autocomplete) => {
                let subreddit = match autocomplete
                    .data
                    .options
                    .iter()
                    .find(|option| option.name == "subreddit")
                {
                    Some(option) => option.value.as_str().unwrap().to_string(),
                    None => {
                        return;
                    }
                };

                if redis_sanitise(&subreddit).len() < 2 {
                    return;
                }

                debug!("Autocomplete for {:?}", subreddit);

                let data_read = ctx.data.read().await;
                let conf = data_read.get::<ConfigStruct>().unwrap();
                let mut con = conf.redis.clone();

                let search = subreddit.replace(' ', "");

                let selector = if ctx.cache.current_user().id.get() == 278550142356029441 {
                    "nsfw"
                } else {
                    "sfw"
                };

                debug!("Getting autocomplete results for {:?}", search);
                let results: Vec<redis::Value> = match redis::cmd("FT.SEARCH")
                    .arg(format!("{}_subs", selector))
                    .arg(format!("*{}*", redis_sanitise(&search)))
                    .arg("LIMIT")
                    .arg(0)
                    .arg(10)
                    .query_async(&mut con)
                    .await
                {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Error getting autocomplete results: {:?}", e);
                        return;
                    }
                };

                let mut option_names = Vec::new();
                for result in results {
                    if let redis::Value::Int(_) = result {
                        continue;
                    }

                    if let Ok(x) = String::from_redis_value(&result) {
                        if x.starts_with("custom_sub") {
                            continue;
                        }
                    }

                    let result: Vec<redis::Value> = match result.into_sequence() {
                        Ok(x) => x,
                        Err(e) => {
                            error!("Error getting autocomplete result: {:?}", e);
                            continue;
                        }
                    };
                    let name = match String::from_redis_value(&result[1]) {
                        Ok(x) => x,
                        Err(e) => {
                            error!("Error getting autocomplete result: {:?}", e);
                            continue;
                        }
                    };
                    option_names.push(name);
                }

                debug!("Autocomplete results: {:?}", option_names);

                let resp = CreateAutocompleteResponse::new().set_choices(
                    option_names
                        .into_iter()
                        .map(|x| AutocompleteChoice::new(x.clone(), x))
                        .collect(),
                );

                match autocomplete
                    .create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp))
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        error!("Failed to send autocomplete response: {:?}", e);
                    }
                };
                return;
            }
            _ => {
                return;
            }
        };

        if let Err(e) = result {
            let report_error;
            let error_message;

            if e.to_string() == "Missing Access".to_string() {
                report_error = false;
                error_message =
                    "The bot does not have permissions to send messages in this channel"
                        .to_string();
            } else {
                report_error = true;
                error_message = format!("An error occurred while processing your request, please report it in the [support server](https://discord.gg/BggYYTpdG5):\n\n{}", e.to_string());
            }

            if report_error {
                capture_anyhow(&e);
                error!("Error: {:?}", e);
            }

            let error_message = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(error_message)
                    .ephemeral(true)
                    .components(vec![CreateActionRow::Buttons(vec![
                        CreateButton::new_link("https://discord.gg/BggYYTpdG5")
                            .label("Support Server")
                            .emoji(ReactionType::Unicode("🛠️".to_string())),
                    ])])
            );

            if let Err(e) = match interaction {
                Interaction::Command(command) => {
                    command.create_response(&ctx.http, error_message).await
                }
                Interaction::Component(component) => {
                    component.create_response(&ctx.http, error_message).await
                }
                Interaction::Modal(modal) => modal.create_response(&ctx.http, error_message).await,
                _ => Err(serenity::Error::Other("Invalid interaction type")),
            } {
                error!("Failed to send error message: {:?}", e);
            }
        };

        transaction.finish();
    }
}

async fn monitor_total_shards(
    shard_manager: Arc<serenity::gateway::ShardManager>,
    total_shards: u32,
) {
    let db_client = redis::Client::open("redis://redis.discord-bot-shared/").unwrap();
    let mut con = db_client
        .get_multiplexed_async_connection()
        .await
        .expect("Can't connect to redis");

    let shard_id: String = env::var("HOSTNAME")
        .expect("HOSTNAME not set")
        .parse()
        .expect("Failed to convert HOSTNAME to string");
    let shard_id: u32 = shard_id
        .replace("discord-shards-", "")
        .parse()
        .expect("unable to convert shard_id to u32");

    loop {
        let _ = sleep(Duration::from_secs(60)).await;

        let db_total_shards: redis::RedisResult<u32> =
            con.get(format!("total_shards_{}", &*NAMESPACE)).await;
        let db_total_shards: u32 =
            db_total_shards.expect("Failed to get or convert total_shards from Redis");

        if !shard_manager.has(ShardId(shard_id)).await {
            debug!(
                "Shard {} not found, marking self for termination.",
                shard_id
            );
            debug!(
                "Instantiated shards: {:?}",
                shard_manager.shards_instantiated().await
            );
            let _ = fs::remove_file("/etc/probes/live");
        } else {
            if !tokio::fs::metadata("/etc/probes/live").await.is_ok() {
                debug!("Resurrected!");
                if !Path::new("/etc/probes").is_dir() {
                    fs::create_dir("/etc/probes").expect("Couldn't create /etc/probes directory");
                }
                let mut file =
                    File::create("/etc/probes/live").expect("Unable to create /etc/probes/live");
                file.write_all(b"alive")
                    .expect("Unable to write to /etc/probes/live");
            }
        }

        if db_total_shards != total_shards {
            debug!(
                "Total shards changed from {} to {}, restarting.",
                total_shards, db_total_shards
            );
            shard_manager.set_shards(shard_id, 1, db_total_shards).await;
            shard_manager
                .initialize()
                .expect("Failed to initialize shard");
        }
    }
}

struct RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    underlying: StubbornIo<T, C>,
}

impl<T, C> AsyncRead for RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match Pin::new(&mut self.underlying).poll_read(cx, buf) {
            Poll::Ready(x) => match x {
                Ok(x) => {
                    return Poll::Ready(Ok(x));
                }
                Err(e) => {
                    warn!("Error with underlying: {}", e);
                    match Box::pin(tokio::time::sleep(Duration::from_millis(500))).poll_unpin(cx) {
                        Poll::Ready(_) => {}
                        Poll::Pending => return Poll::Pending,
                    };
                    return self.poll_read(cx, buf);
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, C> AsyncWrite for RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncWrite + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        match Pin::new(&mut self.underlying).poll_write(cx, buf) {
            Poll::Ready(x) => match x {
                Ok(x) => {
                    return Poll::Ready(Ok(x));
                }
                Err(e) => {
                    warn!("Error with underlying: {}", e);
                    match Box::pin(tokio::time::sleep(Duration::from_millis(500))).poll_unpin(cx) {
                        Poll::Ready(_) => {}
                        Poll::Pending => return Poll::Pending,
                    };
                    return self.poll_write(cx, buf);
                }
            },
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.underlying).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        Pin::new(&mut self.underlying).poll_shutdown(cx)
    }
}

impl<T, C> RetryingTcpStream<T, C>
where
    T: UnderlyingIo<C> + AsyncWrite + AsyncRead,
    C: Clone + Send + Unpin + 'static,
{
    fn new(underlying: StubbornIo<T, C>) -> Self {
        return Self { underlying };
    }
}

fn main() {
    let application_id: u64 = env::var("DISCORD_APPLICATION_ID")
        .expect("DISCORD_APPLICATION_ID not set")
        .parse()
        .expect("Failed to convert application_id to u64");

    let bot_name: std::borrow::Cow<'_, str> = match &application_id {
        278550142356029441 => "booty-bot".into(),
        291255986742624256 => "testing".into(),
        _ => "r-slash".into(),
    };

    let shard_id: String = env::var("HOSTNAME")
        .expect("HOSTNAME not set")
        .parse()
        .expect("Failed to convert HOSTNAME to string");

    let shard_id: u32 = shard_id
        .replace("discord-shards-", "")
        .parse()
        .expect("unable to convert shard_id to u32");

    tracing_subscriber::Registry::default()
        .with(
        tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG)
                .with_filter(tracing_subscriber::filter::FilterFn::new(|meta| {
                    if meta.name().contains("serenity")
                        || meta.target().contains("request")
                        || meta.target().contains("hyper")
                        || meta.target().contains("h2")
                        || meta.target().contains("rustls")
                    {
                        return false;
                    };
                    true
                })),
        )
        .with(sentry::integrations::tracing::layer()
            .span_filter(|meta| {
                if (meta.target().contains("serenity") && meta.target() != "serenity::http::client") || meta.target().contains("h2")
                    || meta.target().contains("hyper") || meta.target().contains("request") || meta.target().contains("tokio") || meta.target().contains("tungstenite") || meta.target().contains("rustls")
                {
                    false
                } else {
                    true
                }
            })
        )
        .init();

    let _guard = sentry::init(("https://e1d0fdcc5e224a40ae768e8d36dd7387@o4504774745718784.ingest.sentry.io/4504793832161280", sentry::ClientOptions {
        release: sentry::release_name!(),
        traces_sample_rate: 0.2,
        environment: Some(bot_name.clone()),
        server_name: Some(shard_id.to_string().into()),
        before_send: Some(Arc::new(|event| {
            if let Some(x) = &event.transaction {
                if x.contains("recv") || x.contains("recv_event") || x.contains("dispatch") || x.contains("handle_event") || x.contains("check_heartbeat") || x.contains("headers") {
                    return None;
                }
            };
            return Some(event);
        })),
        ..Default::default()
    }.add_integration(sentry::integrations::backtrace::AttachStacktraceIntegration::new())
)
);

    tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .unwrap()
    .block_on(async {
        println!("Starting up...");
        
        trace!("TRACE");
        let token = env::var("DISCORD_TOKEN").expect("DISCORD_TOKEN not set");

        println!("Connecting to redis...");
        let redis_client =
            redis::Client::open("redis://redis.discord-bot-shared.svc.cluster.local/").unwrap();
        let mut con = redis_client
            .get_multiplexed_async_connection()
            .await
            .expect("Can't connect to redis");
        println!("Connected to redis");

        let posthog_key: String = env::var("POSTHOG_API_KEY")
            .expect("POSTHOG_API_KEY not set")
            .parse()
            .expect("Failed to convert POSTHOG_API_KEY to string");
        let posthog = posthog::Client::new(posthog_key, "https://eu.i.posthog.com".to_string());

        println!("Connecting to mongodb...");
        let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
        println!("Connected to mongodb");
        client_options.app_name = Some(format!("Shard {}", shard_id));

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

        let nsfw_subreddits: Vec<String> = doc
            .get_array("nsfw")
            .unwrap()
            .into_iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect();

        let total_shards: redis::RedisResult<u32> =
            con.get(format!("total_shards_{}", &*NAMESPACE)).await;
        let total_shards: u32 = total_shards.expect("Failed to get or convert total_shards");

        println!("Booting with {:?} total shards", total_shards);

        let mut client = serenity::Client::builder(token, GatewayIntents::GUILDS)
            .event_handler(Handler)
            .application_id(application_id.into())
            .await
            .expect("Error creating client");

        println!("Created client");
        {
            let mut data: tokio::sync::RwLockWriteGuard<'_, TypeMap> = client.data.write().await;

            println!("Connecting to post subscriber...");
            let reconnect_opts = ReconnectOptions::new()
                .with_exit_if_first_connect_fails(false)
                .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
            let tcp_stream = RetryingTcpStream::new(
                StubbornTcpStream::connect_with_options(
                    "post-subscriber.discord-bot-shared.svc.cluster.local:50051",
                    reconnect_opts,
                )
                .await
                .expect("Failed to connect to post subscriber"),
            );
            let transport = Transport::from((tcp_stream, Bincode::default()));

            let subscriber =
                post_subscriber::SubscriberClient::new(tarpc::client::Config::default(), transport)
                    .spawn();

            println!("Connected to post subscriber");

            println!("Connecting to auto poster...");
            let reconnect_opts = ReconnectOptions::new()
                .with_exit_if_first_connect_fails(false)
                .with_retries_generator(|| iter::repeat(Duration::from_secs(1)));
            let tcp_stream = RetryingTcpStream::new(
                StubbornTcpStream::connect_with_options(
                    "auto-poster.discord-bot-shared.svc.cluster.local:50051",
                    reconnect_opts,
                )
                .await
                .expect("Failed to connect to autoposter"),
            );
            let transport = Transport::from((tcp_stream, Bincode::default()));

            let auto_poster =
                auto_poster::AutoPosterClient::new(tarpc::client::Config::default(), transport).spawn();

            println!("Connected to auto poster");

            data.insert::<ConfigStruct>(ConfigStruct {
                shard_id,
                nsfw_subreddits,
                redis: con,
                mongodb: mongodb_client,
                posthog,
            });
            data.insert::<post_subscriber::SubscriberClient>(subscriber);
            data.insert::<auto_poster::AutoPosterClient>(auto_poster);
        }

        let shard_manager = client.shard_manager.clone();
        tokio::spawn(async move {
            println!("Spawning shard monitor thread");
            monitor_total_shards(shard_manager, total_shards).await;
        });

        let thread = tokio::spawn(async move {

            println!("Spawning client thread");
            client
                .start_shard(shard_id, total_shards)
                .await
                .expect("Failed to start shard");
        });

        // If client thread exits, shard has crashed, so mark self as unhealthy.
        match thread.await {
            Ok(_) => {}
            Err(_) => {
                fs::remove_file("/etc/probes/live").expect("Unable to remove /etc/probes/live");
            }
        }
    });
}
