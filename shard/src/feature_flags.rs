use serde_json::Value;
use std::collections::HashMap;
use tokio::sync::RwLock;
use std::sync::Arc;
use serenity::all::{Command, CommandOption, CommandOptionChoice, CommandOptionType, CommandType, CreateCommand, CreateCommandOption, GuildId};
use tokio::time::Instant;
use anyhow::Result;

const CACHE_DURATION: u64 = 60 * 60 * 12; // 12 hours

struct CommandOptionChoiceTemplate {
    name: String,
    value: Value,
}

impl PartialEq<CommandOptionChoice> for CommandOptionChoiceTemplate {
    fn eq(&self, other: &CommandOptionChoice) -> bool {
        self.name == other.name && self.value == other.value
    }
}

struct CommandOptionTemplate {
    name: String,
    description: String,
    required: bool,
    choices: Vec<CommandOptionChoiceTemplate>,
    kind: CommandOptionType,
    sub_options: Vec<CommandOptionTemplate>,
}

impl PartialEq<CommandOption> for &CommandOptionTemplate {
    fn eq(&self, other: &CommandOption) -> bool {
        if self.name != other.name {
            return false;
        }

        if self.description != other.description {
            return false;
        }

        if self.required != other.required {
            return false;
        }

        if self.choices != other.choices {
            return false;
        }

        if self.kind != other.kind {
            return false;
        }

        if self.sub_options != other.options {
            return false;
        }

        true
    }
}

impl PartialEq<CommandOption> for CommandOptionTemplate {
    fn eq(&self, other: &CommandOption) -> bool {
        self == other
    }
}

impl Into<CreateCommandOption> for &CommandOptionTemplate {
    fn into(self) -> CreateCommandOption {
        let mut command_option = CreateCommandOption::new(self.kind, self.name.clone(), self.description.clone());
        command_option = command_option
            .required(self.required);

        for choice in &self.choices {
            match choice.value.clone() {
                Value::String(value) => command_option = command_option.add_string_choice(choice.name.clone(), value),
                Value::Number(value) => command_option = command_option.add_number_choice(choice.name.clone(), value.as_f64().unwrap()),
                _ => {}
            }
        }

        for sub_option in &self.sub_options {
            command_option = command_option.add_sub_option(sub_option.into());
        }

        command_option
    }
}

struct CommandTemplate {
    name: String,
    description: String,
    options: Vec<CommandOptionTemplate>,
    kind: CommandType,
}

impl PartialEq<Command> for &CommandTemplate {
    fn eq(&self, other: &Command) -> bool {
        if self.name != other.name {
            return false;
        }

        if self.description != other.description {
            return false;
        }

        if self.options != other.options {
            return false;
        }

        if self.kind != other.kind {
            return false;
        }

        true
    }
}


impl Into<CreateCommand> for &CommandTemplate {
    fn into(self) -> CreateCommand {
        let mut command = CreateCommand::new(self.name.clone());
        command = command
            .description(self.description.clone())
            .kind(self.kind);

        for option in &self.options {
            let mut command_option = CreateCommandOption::new(option.kind, option.name.clone(), option.description.clone());
            command_option = command_option
                .required(option.required);

            for choice in &option.choices {
                match choice.value.clone() {
                    Value::String(value) => command_option = command_option.add_string_choice(choice.name.clone(), value),
                    Value::Number(value) => command_option = command_option.add_number_choice(choice.name.clone(), value.as_f64().unwrap()),
                    _ => {}
                }
            }

            for sub_option in &option.sub_options {
                command_option = command_option.add_sub_option(sub_option.into());
            }

            command = command.add_option(command_option);
        }

        command
    }
}

pub struct Manager {
    client: posthog::Client,
    /// When feature flags need to be re-fetched for guild
    flags_cache: Arc<RwLock<HashMap<GuildId, Instant>>>,
    discord_http: Arc<serenity::http::Http>,
    /// {flag_name: {command_name: command}}
    templates: HashMap<String, HashMap<String, CommandTemplate>>,
}

impl Manager {
    pub fn new(client: posthog::Client, http: Arc<serenity::http::Http>) -> Self {
        let templates: HashMap<String, HashMap<String, CommandTemplate>> = HashMap::new();

        Self {
            client,
            flags_cache: Arc::new(RwLock::new(HashMap::new())),
            discord_http: http,
            templates,
        }
    }


    /// Update a guild's commands if cache is outdated and commands have changed
    pub async fn update_guild(&self, guild_id: &GuildId) -> Result<()> {
        // If cache not outdated, return
        let flags_cache = self.flags_cache.read().await;
        if let Some(update_time) = flags_cache.get(guild_id) {
            if Instant::now() < *update_time {
                return Ok(());
            }
        };
        // Drop read lock before sending web requests
        drop(flags_cache);

        // Fetch feature flags from PostHog
        let flags = self.client.get_feature_flags(&format!("guild_{}", guild_id), Some(guild_id.get())).await?;
        let mut expected_commands = Vec::new();
        for (flag, value) in flags {
            if let Some(flag_templates) = self.templates.get(&flag) {
                if let Some(template) = flag_templates.get(&value) {
                    expected_commands.push(template);
                }
            }
        }

        let guild_commands = guild_id.get_commands(&self.discord_http).await?;

        // Exit if commands are the same
        if expected_commands == guild_commands {
            return Ok(());
        }

        let expected_commands: Vec<CreateCommand> = expected_commands.into_iter().map(|command| command.into()).collect();

        // Update commands
        guild_id.set_commands(&self.discord_http, expected_commands).await?;

        // Update cache
        let mut flags_cache = self.flags_cache.write().await;
        flags_cache.insert(*guild_id, Instant::now() + std::time::Duration::from_secs(CACHE_DURATION));

        Ok(())
    }
}
