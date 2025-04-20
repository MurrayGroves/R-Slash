use mongodb::bson::Bson;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct PostRequest {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    // Interval between posts in seconds
    pub interval: Duration,
    pub limit: Option<u32>,
    pub current: u32,
    pub last_post: Instant,
    pub author: UserId,
}

pub enum AutoPostCommand {
    Start(PostRequest),
    Stop(ChannelId),
}

use serde::{Deserialize, Serialize};
use serenity::all::{
    ChannelId, CreateActionRow, CreateEmbed, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage,
    CreateModal, UserId,
};
use strum::{Display, EnumIter};
use tokio::time::Instant;

/// Stores config values required for operation of the shard
#[derive(Debug, Clone)]
pub struct ConfigStruct {
    pub shard_id: u32,
    pub nsfw_subreddits: Vec<String>,
    pub redis: redis::aio::MultiplexedConnection,
    pub mongodb: mongodb::Client,
    pub posthog: posthog::Client,
}

impl serenity::prelude::TypeMapKey for ConfigStruct {
    type Value = ConfigStruct;
}

#[derive(Debug, Clone)]
pub struct InteractionResponseMessage {
    pub file: Option<serenity::builder::CreateAttachment>,
    pub embed: Option<serenity::all::CreateEmbed>,
    pub content: Option<String>,
    pub ephemeral: bool,
    pub components: Option<Vec<CreateActionRow>>,
    pub fallback: ResponseFallbackMethod,
}

impl Into<CreateInteractionResponseMessage> for InteractionResponseMessage {
    fn into(self) -> CreateInteractionResponseMessage {
        let mut resp = CreateInteractionResponseMessage::new();
        if let Some(embed) = self.embed {
            resp = resp.embed(embed);
        };

        if let Some(components) = self.components {
            resp = resp.components(components);
        };

        if let Some(content) = self.content {
            resp = resp.content(content);
        };

        resp.ephemeral(self.ephemeral)
    }
}

impl Into<CreateInteractionResponseFollowup> for InteractionResponseMessage {
    fn into(self) -> CreateInteractionResponseFollowup {
        let mut resp = CreateInteractionResponseFollowup::new();
        if let Some(embed) = self.embed {
            resp = resp.embed(embed);
        };

        if let Some(components) = self.components {
            resp = resp.components(components);
        };

        if let Some(content) = self.content {
            resp = resp.content(content);
        };

        resp.ephemeral(self.ephemeral)
    }
}

impl Into<CreateInteractionResponse> for InteractionResponseMessage {
    fn into(self) -> CreateInteractionResponse {
        CreateInteractionResponse::Message(self.into())
    }
}

#[derive(Debug, Clone)]
pub enum InteractionResponse {
    Message(InteractionResponseMessage),
    Modal(CreateModal),
    None,
}

impl TryInto<CreateMessage> for InteractionResponse {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<CreateMessage, Self::Error> {
        match self {
            InteractionResponse::Message(message) => {
                let mut resp = CreateMessage::new();
                if let Some(embed) = message.embed {
                    resp = resp.embed(embed);
                };

                if let Some(content) = message.content {
                    resp = resp.content(content);
                };

                if let Some(components) = message.components {
                    resp = resp.components(components);
                };

                Ok(resp)
            }
            _ => Err(anyhow::anyhow!("Invalid response type")),
        }
    }
}

impl Default for InteractionResponse {
    fn default() -> Self {
        InteractionResponse::Message(InteractionResponseMessage {
            file: None,
            embed: None,
            content: None,
            ephemeral: false,
            components: None,
            fallback: ResponseFallbackMethod::Error,
        })
    }
}

impl Default for InteractionResponseMessage {
    fn default() -> Self {
        InteractionResponseMessage {
            file: None,
            embed: None,
            content: None,
            ephemeral: false,
            components: None,
            fallback: ResponseFallbackMethod::Error,
        }
    }
}

// What to do if sending a response fails due to already being acknowledged
#[derive(Debug, Clone)]
pub enum ResponseFallbackMethod {
    /// Edit the original response
    Edit,
    /// Send a followup response
    Followup,
    /// Return an error
    Error,
    /// Do nothing
    None,
}

#[macro_export]
macro_rules! span_filter {
    ($meta:expr, $cx:expr) => {
        const TARGETS: [&str; 8] = [
            "discord_shard",
            "post_api",
            "post_subscriber",
            "reddit_downloader",
            "auto_poster",
            "memberships",
            "posthog",
            "rslash_common",
        ];
        //const BAD_TARGETS: [&str;2] = ["runtime", "tokio"];
        const BAD_TARGETS: [&str; 4] = ["runtime", "hyper", "tokio", "h2"];
        let tgt = $meta.target();

        for bad_target in BAD_TARGETS.iter() {
            if tgt.contains(bad_target) {
                return false;
            }
        }

        for target in TARGETS.iter() {
            if tgt.contains(target) {
                return true;
            }
        }

        if let Some(current_span) = $cx.lookup_current() {
            let tgt = current_span.metadata().target();
            for target in TARGETS.iter() {
                if tgt.contains(target) {
                    return true;
                }
            }
        }

        return false;
    };
}

pub fn error_response(error_title: &str, error_desc: &str) -> InteractionResponse {
    InteractionResponse::Message(InteractionResponseMessage {
        embed: Some(
            CreateEmbed::default()
                .title(error_title)
                .description(error_desc)
                .color(0xff0000)
                .to_owned(),
        ),
        ..Default::default()
    })
}

#[macro_export]
macro_rules! initialise_observability {
	($service_name: literal) => {initialise_observability!($service_name,)};
    ($service_name:literal, $(($key:literal, $value:ident),)*) => {
        let trace_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint("http://100.67.30.19:4317")
            .build().expect("Failed to create trace exporter");

        let tracing_provider = trace::SdkTracerProvider::builder()
            .with_batch_exporter(trace_exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name($service_name)
					$(.with_attribute(KeyValue::new($key, $value.to_string())))*
                    .build(),
            )
            .build();

		let log_exporter = opentelemetry_otlp::LogExporter::builder()
			.with_tonic()
			.with_endpoint("http://100.67.30.19:4317")
			.build().expect("Failed to create log exporter");

		let logging_provider = logs::SdkLoggerProvider::builder()
			.with_batch_exporter(log_exporter)
			.with_resource(
				opentelemetry_sdk::Resource::builder()
					.with_service_name($service_name)
					$(.with_attribute(KeyValue::new($key, $value.to_string())))*
					.build(),
			)
			.build();

		let tracer = opentelemetry::trace::TracerProvider::tracer(&tracing_provider, $service_name);
		global::set_tracer_provider(tracing_provider.clone());
		let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);

		let filter_otel = EnvFilter::new("trace")
			.add_directive("hyper=off".parse().unwrap())
			.add_directive("opentelemetry=off".parse().unwrap())
			.add_directive("tonic=off".parse().unwrap())
			.add_directive("h2=off".parse().unwrap())
			.add_directive("tarpc=off".parse().unwrap())
			.add_directive("redis=off".parse().unwrap())
			.add_directive("mongodb=off".parse().unwrap())
			.add_directive("tower=off".parse().unwrap())
			.add_directive("runtime=off".parse().unwrap())
			.add_directive("tokio=off".parse().unwrap())
			.add_directive("serenity=off".parse().unwrap())
			.add_directive("tungstenite=off".parse().unwrap())
			.add_directive("reqwest=off".parse().unwrap());

		let otel_layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&logging_provider).with_filter(filter_otel);

		tracing_subscriber::Registry::default()
			.with(telemetry.with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
				span_filter!(meta, cx);
			}))) // Tracing layer
			.with(
				tracing_subscriber::fmt::layer()
					.compact()
					.with_ansi(false)
					.with_filter(tracing_subscriber::filter::LevelFilter::DEBUG)
					.with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
						span_filter!(meta, cx);
					})),
			) // STDOUT Layer
			.with(otel_layer.with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
				span_filter!(meta, cx);
			}))) // Logging Layer
			.with(sentry::integrations::tracing::layer()
				.with_filter(tracing_subscriber::filter::DynFilterFn::new(|meta, cx| {
					span_filter!(meta, cx);
				}))
			) // Sentry Layer
			.init();
    };
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Hash, EnumIter, Eq, Display)]
pub enum Bot {
    BB,
    RS,
}

impl Into<Bson> for Bot {
    fn into(self) -> Bson {
        match self {
            Bot::BB => Bson::String("BB".to_string()),
            Bot::RS => Bson::String("RS".to_string()),
        }
    }
}

impl FromStr for Bot {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bb" => Ok(Bot::BB),
            "rs" => Ok(Bot::RS),
            _ => Err(format!("Invalid bot name: {}", s)),
        }
    }
}
