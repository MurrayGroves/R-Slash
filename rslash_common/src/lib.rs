pub mod access_tokens;
pub mod rpc;

pub use access_tokens::Limiter;

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
    ChannelId, CreateEmbed, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateMessage, CreateModal, UserId,
};
use serenity::builder::CreateComponent;
use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct InteractionResponseMessage<'a> {
    pub file: Option<serenity::builder::CreateAttachment<'a>>,
    pub embed: Option<CreateEmbed<'a>>,
    pub content: Option<String>,
    pub ephemeral: bool,
    pub components: Option<Vec<CreateComponent<'a>>>,
    pub fallback: ResponseFallbackMethod,
}

impl<'a> Into<CreateInteractionResponseMessage<'a>> for InteractionResponseMessage<'a> {
    fn into(self) -> CreateInteractionResponseMessage<'a> {
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

impl<'a> Into<CreateInteractionResponseFollowup<'a>> for InteractionResponseMessage<'a> {
    fn into(self) -> CreateInteractionResponseFollowup<'a> {
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

impl<'a> Into<CreateInteractionResponse<'a>> for InteractionResponseMessage<'a> {
    fn into(self) -> CreateInteractionResponse<'a> {
        CreateInteractionResponse::Message(self.into())
    }
}

#[derive(Debug, Clone)]
pub enum InteractionResponse<'a> {
    Message(InteractionResponseMessage<'a>),
    Modal(CreateModal<'a>),
    None,
}

impl<'a> TryInto<CreateMessage<'a>> for InteractionResponse<'a> {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<CreateMessage<'a>, Self::Error> {
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

impl Default for InteractionResponse<'_> {
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

impl Default for InteractionResponseMessage<'_> {
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

pub fn error_response<'a>(
    error_title: String,
    error_desc: String,
) -> CreateInteractionResponse<'a> {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new().embed(
            CreateEmbed::default()
                .title(error_title)
                .description(error_desc)
                .color(0xff0000)
                .to_owned(),
        ),
    )
}

pub fn error_message<'a>(error_title: String, error_desc: String) -> CreateMessage<'a> {
    CreateMessage::new().embed(
        CreateEmbed::default()
            .title(error_title)
            .description(error_desc)
            .color(0xff0000)
            .to_owned(),
    )
}

#[macro_export]
macro_rules! initialise_observability {
	($service_name: literal) => {initialise_observability!($service_name,)};
    ($service_name:literal, $(($key:literal, $value:ident),)*) => {
        let trace_exporter = opentelemetry_otlp::SpanExporterBuilder::Tonic(opentelemetry_otlp::TonicExporterBuilder::default().with_endpoint("http://100.67.30.19:4317"))
            .build_span_exporter().expect("Failed to create trace exporter");

        let tracing_provider = trace::TracerProvider::builder()
            .with_batch_exporter(trace_exporter, opentelemetry_sdk::runtime::Tokio)
			.with_config(opentelemetry_sdk::trace::Config::default().with_resource(
                opentelemetry_sdk::Resource::new(vec![opentelemetry::KeyValue::new("service.name", $service_name)$(,opentelemetry::KeyValue::new($key, $value.to_string()))*] )
            ))

            .build();

		let log_exporter = opentelemetry_otlp::LogExporterBuilder::Tonic(opentelemetry_otlp::TonicExporterBuilder::default().with_endpoint("http://100.67.30.19:4317"))
			.build_log_exporter().expect("Failed to create log exporter");

		let logging_provider = logs::LoggerProvider::builder()
			.with_batch_exporter(log_exporter, opentelemetry_sdk::runtime::Tokio)
			.with_resource(
                opentelemetry_sdk::Resource::new(vec![opentelemetry::KeyValue::new("service.name", $service_name)$(,opentelemetry::KeyValue::new($key, $value.to_string()))*] )
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
				if meta.level() <= &tracing::Level::INFO  {
					return false;
				}
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

#[derive(Serialize, Deserialize, Debug)]
pub enum SubredditStatus {
    Valid,
    Invalid(String),
}
