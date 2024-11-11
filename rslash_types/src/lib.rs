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

use serenity::all::{
	ChannelId, CreateActionRow, CreateInteractionResponse, CreateInteractionResponseFollowup,
	CreateInteractionResponseMessage, CreateMessage, CreateModal, UserId,
};
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

#[macro_export] macro_rules! span_filter {
    ($meta:expr, $cx:expr) => {
        const TARGETS: [&str; 8] = ["discord_shard", "post_api", "post_subscriber", "reddit_downloader", "auto_poster", "memberships", "posthog", "rslash_types"];
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
    }
}