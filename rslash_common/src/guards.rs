// Return error interaction response from current function if bot doesn't have permission to send messages in the channel
#[macro_export]
macro_rules! error_if_no_send_message_perm {
    ($ctx:expr, $guild_id:expr, $tracker:expr) => {
        if let Some(guild) = $guild_id {
            if let Ok(user) = guild.current_user_member(&$ctx.http).await {
                if let Some(permissions) = user.permissions {
                    if !permissions.send_messages() {
                        return $tracker
                            .send_response(CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().embed(
                                    CreateEmbed::default()
                                        .title("Permission Error")
                                        .description(
                                            "I don't have permission to send messages in this channel.",
                                        )
                                        .color(0xff0000)
                                        .to_owned(),
                                ).
                                ephemeral(true)))
                            .await;
                    }
                }

            }
        }
    }
}

#[macro_export]
macro_rules! error_if_no_premium {
    ($ctx:expr, $user:expr, $tracker:expr) => {
        let tiers =
            get_user_tiers($user.to_string(), $ctx.data::<ShardState>().mongodb.clone()).await;
        if !tiers.bronze.active {
            return $tracker
                .send_response(CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new().embed(
                        CreateEmbed::default()
                            .title("Premium Feature")
                            .description(
                                "You must have premium in order to use this command.
                    Get it [here](https://ko-fi.com/rslash)",
                            )
                            .color(0xff0000)
                            .to_owned(),
                    ),
                ))
                .await;
        }
    };
}

#[macro_export]
macro_rules! error_if_user_no_manage_channels_perm {
    ($ctx:expr, $member:expr, $tracker:expr) => {
        if let Some(member) = $member && let Some(perms) = member.permissions {
            if !perms.manage_channels() {
                return $tracker
                    .send_response(CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .embed(
                                CreateEmbed::default()
                                    .title("Permission Error")
                                    .description(
                                        "You need the `Manage Channels` permission to do this!",
                                    )
                                    .color(0xff0000)
                                    .to_owned(),
                            )
                            .ephemeral(true),
                    ))
                    .await;
            }
        }
    };
}
