pub mod access_tokens;
mod guards;
pub mod rpc;

pub use access_tokens::Limiter;
use redis::aio::MultiplexedConnection;
use redis::{FromRedisValue, RedisError};
use user_config_manager::TextAllowLevel;

use std::collections::HashMap;
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
use tracing::{debug, trace};

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
        rustls::crypto::ring::default_provider().install_default().expect("Failed to install rustls crypto provider");

        let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
        builder.install().expect("Failed to install Prometheus exporter");

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

/// Represents a Reddit post stored in the database.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Post {
    /// Reddit ID, not database ID
    pub id: String,
    pub author: String,
    pub title: String,
    /// The URL of the post on Reddit
    pub url: String,
    /// The converted embed URLs
    pub embed_urls: Vec<String>,
    /// The timestamp when the post was made on Reddit
    pub timestamp: u64,
    /// The score of the post
    pub score: isize,
    /// Body text of the post, if present
    pub text: Option<String>,
    /// URL the post links to, if it wasn't embeddable
    pub linked_url: Option<String>,
    /// Link to the image to use when displaying the linked_url
    pub linked_url_image: Option<String>,
    pub linked_url_title: Option<String>,
    pub linked_url_description: Option<String>,
}

impl FromRedisValue for Post {
    fn from_redis_value(v: &redis::Value) -> redis::RedisResult<Self> {
        let post: HashMap<String, String> = redis::from_redis_value(v)?;

        if post.is_empty() {
            return Err(RedisError::from((
                redis::ErrorKind::ParseError,
                "Post data is empty",
            )));
        }

        let id = post.get("id").ok_or(RedisError::from((
            redis::ErrorKind::ParseError,
            "id missing",
        )))?;

        let author = post.get("author").ok_or(RedisError::from((
            redis::ErrorKind::ParseError,
            "author missing",
        )))?;

        let title = post.get("title").ok_or(RedisError::from((
            redis::ErrorKind::ParseError,
            "title missing",
        )))?;

        let url = post.get("url").ok_or(RedisError::from((
            redis::ErrorKind::ParseError,
            "url missing",
        )))?;

        let embed_urls: Vec<String> = post
            .get("embed_url")
            .ok_or(RedisError::from((
                redis::ErrorKind::ParseError,
                "embed_url missing",
            )))?
            .split(",")
            .into_iter()
            .filter(|x| !x.is_empty())
            .map(|s| {
                if !s.starts_with("http") {
                    format!("https://cdn.rsla.sh/gifs/{}", s)
                } else {
                    s.to_string()
                }
            })
            .collect();
        let timestamp = post
            .get("timestamp")
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    RedisError::from((
                        redis::ErrorKind::ParseError,
                        "timestamp must be a valid u64",
                    ))
                })
            })
            .ok_or(RedisError::from((
                redis::ErrorKind::ParseError,
                "timestamp missing",
            )))??;

        let score = post
            .get("score")
            .map(|s| {
                s.parse::<isize>().map_err(|_| {
                    RedisError::from((redis::ErrorKind::ParseError, "score must be a valid isize"))
                })
            })
            .ok_or(RedisError::from((
                redis::ErrorKind::ParseError,
                "score missing",
            )))??;

        Ok(Post {
            id: id.to_string(),
            title: title.to_string(),
            url: url.to_string(),
            text: post.get("text").cloned(),
            linked_url: post.get("linked_url").cloned(),
            linked_url_image: post.get("linked_url_image").cloned(),
            linked_url_description: post.get("linked_url_description").cloned(),
            linked_url_title: post.get("linked_url_title").cloned(),
            timestamp,
            score,
            author: author.to_string(),
            embed_urls,
        })
    }
}

impl From<Post> for Vec<(String, String)> {
    #[tracing::instrument]
    fn from(post: Post) -> Vec<(String, String)> {
        let mut vec = vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url),
            ("title".to_string(), post.title),
            ("author".to_string(), post.author),
            ("id".to_string(), post.id),
            ("timestamp".to_string(), post.timestamp.to_string()),
            ("embed_url".to_string(), post.embed_urls.join(",")),
        ];

        if let Some(text) = post.text {
            vec.push(("text".to_string(), text))
        }

        if let Some(url) = post.linked_url {
            vec.push(("linked_url".to_string(), url))
        }

        if let Some(image) = post.linked_url_image {
            vec.push(("linked_url_image".to_string(), image))
        }

        if let Some(description) = post.linked_url_description {
            vec.push(("linked_url_description".to_string(), description))
        }

        if let Some(title) = post.linked_url_title {
            vec.push(("linked_url_title".to_string(), title))
        }

        vec
    }
}

impl From<&Post> for Vec<(String, String)> {
    #[tracing::instrument]
    fn from(post: &Post) -> Vec<(String, String)> {
        let mut vec = vec![
            ("score".to_string(), post.score.to_string()),
            ("url".to_string(), post.url.to_string()),
            ("title".to_string(), post.title.to_string()),
            ("author".to_string(), post.author.to_string()),
            ("id".to_string(), post.id.to_string()),
            ("timestamp".to_string(), post.timestamp.to_string()),
            ("embed_url".to_string(), post.embed_urls.join(",")),
        ];

        if let Some(text) = &post.text {
            vec.push(("text".to_string(), text.to_string()))
        }

        if let Some(url) = &post.linked_url {
            vec.push(("linked_url".to_string(), url.to_string()))
        }

        if let Some(image) = &post.linked_url_image {
            vec.push(("linked_url_image".to_string(), image.to_string()))
        }

        if let Some(description) = &post.linked_url_description {
            vec.push((
                "linked_url_description".to_string(),
                description.to_string(),
            ))
        }

        if let Some(title) = &post.linked_url_title {
            vec.push(("linked_url_title".to_string(), title.to_string()))
        }

        vec
    }
}

impl Post {
    pub fn get_text_level(&self) -> TextAllowLevel {
        let has_media = !self.embed_urls.is_empty();

        let has_text = self.text.is_some() || self.linked_url.is_some();

        if has_media && has_text {
            TextAllowLevel::Both
        } else if has_media {
            TextAllowLevel::MediaOnly
        } else {
            TextAllowLevel::TextOnly
        }
    }
}

pub async fn get_post_content_type(
    con: &mut MultiplexedConnection,
    post_id: &str,
) -> Result<TextAllowLevel, anyhow::Error> {
    trace!("Getting content type for {:?}", post_id);
    let (media, text, linked_url): (Option<String>, Option<String>, Option<String>) = redis::pipe()
        .hget(post_id, "embed_url")
        .hget(post_id, "text")
        .hget(post_id, "linked_url")
        .query_async(con)
        .await?;

    let has_media = if let Some(urls) = media {
        if urls.is_empty() { false } else { true }
    } else {
        false
    };

    let has_text = text.is_some() || linked_url.is_some();

    if has_media && has_text {
        Ok(TextAllowLevel::Both)
    } else if has_media {
        Ok(TextAllowLevel::MediaOnly)
    } else {
        Ok(TextAllowLevel::TextOnly)
    }
}
