use crate::discord::ResponseTracker;
use crate::{CreateEmbed, ShardState};
use serenity::all::{
    ChannelId, CommandInteraction, ComponentInteraction, ComponentInteractionDataKind, Context,
    CreateActionRow, CreateComponent, CreateSelectMenuKind, MessageFlags,
};
use std::borrow::Cow;
use std::collections::HashMap;
use tracing::instrument;

use indoc::indoc;

use anyhow::{Result, anyhow, bail};
use rslash_common::error_if_user_no_manage_channels_perm;
use serde_json::json;
use serenity::builder::{
    CreateContainer, CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu,
    CreateSelectMenuOption, CreateTextDisplay,
};
use user_config_manager::{TextAllowLevel, get_channel_config, save_channel_config};

#[instrument(skip(command, ctx, tracker))]
pub async fn configure_channel<'a>(
    command: &'a CommandInteraction,
    ctx: &Context,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    error_if_user_no_manage_channels_perm!(ctx, &command.member, tracker);

    let channel_config = get_channel_config(
        &mut ctx.data::<ShardState>().mongodb.clone(),
        ChannelId::new(command.channel_id.get()),
    )
    .await?;

    let components = vec![CreateComponent::Container(CreateContainer::new(vec![
        CreateComponent::TextDisplay(CreateTextDisplay::new(indoc! {"
						# Text/Media Level
						Which types of posts to send in this channel.
						- Text Only - Send only posts containing exclusively text or links
						- Media Only - Send only posts containing exclusively images/videos/gifs
						- Both - Send posts containing any mix of media and text
					"})),
        CreateComponent::ActionRow(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                json!({
                    "command": "configure_channel",
                    "field": "text_allow_level"
                })
                .to_string(),
                CreateSelectMenuKind::String {
                    options: Cow::from(vec![
                        CreateSelectMenuOption::new("Text Only", "TextOnly"),
                        CreateSelectMenuOption::new("Media Only", "MediaOnly"),
                        CreateSelectMenuOption::new("Both", "Both"),
                    ]),
                },
            )
            .placeholder(match channel_config.text_allowed.unwrap_or_default() {
                TextAllowLevel::TextOnly => "Text Only",
                TextAllowLevel::MediaOnly => "Media Only",
                TextAllowLevel::Both => "Both",
            })
            .min_values(1)
            .max_values(1),
        )),
    ]))];

    let configurator = CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .flags(MessageFlags::IS_COMPONENTS_V2)
            .components(components)
            .ephemeral(true),
    );

    tracker.send_response(configurator).await?;

    Ok(())
}

#[instrument(skip(ctx, interaction, tracker))]
pub async fn configure_channel_component_handler<'a>(
    ctx: &Context,
    interaction: &ComponentInteraction,
    custom_data: HashMap<String, serde_json::Value>,
    mut tracker: ResponseTracker<'a>,
) -> Result<()> {
    let field = custom_data
        .get("field")
        .ok_or(anyhow!("Field not present"))?
        .as_str()
        .ok_or(anyhow!("Field value not string"))?;

    let value = match &interaction.data.kind {
        ComponentInteractionDataKind::StringSelect { values } => {
            values.get(0).ok_or(anyhow!("No value selected"))?.clone()
        }
        _ => {
            bail!("Invalid interaction data kind");
        }
    };

    let mut mongodb = ctx.data::<ShardState>().mongodb.clone();

    let mut config =
        get_channel_config(&mut mongodb, ChannelId::new(interaction.channel_id.get())).await?;

    match field {
        "text_allow_level" => {
            config.text_allowed = Some(match value.as_str() {
                "TextOnly" => TextAllowLevel::TextOnly,
                "MediaOnly" => TextAllowLevel::MediaOnly,
                "Both" => TextAllowLevel::Both,
                _ => bail!("Unrecognised value"),
            })
        }
        _ => bail!("Unknown field type"),
    }

    save_channel_config(&mut mongodb, &config).await?;

    tracker.send_acknowledge().await?;

    ctx.data::<ShardState>()
        .posthog
        .capture(
            "configure_channel",
            serde_json::to_value(config)?,
            interaction.guild_id,
            Some(interaction.channel_id),
            &format!("user_{}", interaction.user.id),
        )
        .await?;

    Ok(())
}
