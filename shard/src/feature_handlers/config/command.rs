use crate::discord::ResponseTracker;
use crate::feature_handlers::config::channel_config::configure_channel;
use anyhow::anyhow;
use serenity::all::{CommandInteraction, Context};
use tracing::instrument;

#[instrument(skip(command, ctx, tracker))]
pub async fn config<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    tracker: ResponseTracker<'a>,
) -> anyhow::Result<()> {
    let options = &command.data.options;
    if let Some(first) = options.get(0) {
        match first.name.as_str() {
            "channel" => configure_channel(command, ctx, tracker).await,
            _ => Err(anyhow!("Unknown subcommand")),
        }
    } else {
        Err(anyhow!("No subcommand"))
    }
}
