use std::sync::Arc;

use rslash_types::{InteractionResponse, InteractionResponseMessage};

use serenity::all::{
    CreateInteractionResponse, CreateInteractionResponseFollowup, CreateModal, Http, Interaction,
};

use anyhow::{anyhow, Result};
use tracing::{debug, instrument};

pub struct ResponseTracker<'a> {
    pub interaction: &'a Interaction,
    pub sent_response: bool,
    http: Arc<Http>,
}

impl<'a> ResponseTracker<'a> {
    pub fn new(interaction: &'a Interaction, http: Arc<Http>) -> Self {
        Self {
            interaction,
            sent_response: false,
            http,
        }
    }

    async fn send_followup(&mut self, response: InteractionResponseMessage) -> Result<()> {
        let message: CreateInteractionResponseFollowup = response.into();

        match &self.interaction {
            Interaction::Command(command) => {
                command
                    .create_followup(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Component(component) => {
                component
                    .create_followup(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Modal(modal) => {
                modal
                    .create_followup(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            _ => {
                return Err(anyhow!("Invalid interaction type"));
            }
        }

        Ok(())
    }

    async fn send_message(&mut self, response: InteractionResponseMessage) -> Result<()> {
        self.sent_response = true;
        let message: CreateInteractionResponse = response.into();

        match &self.interaction {
            Interaction::Command(command) => {
                command
                    .create_response(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Component(component) => {
                component
                    .create_response(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Modal(modal) => {
                modal
                    .create_response(&self.http, message)
                    .await
                    .map(|_| ())?;
            }
            _ => {
                return Err(anyhow!("Invalid interaction type"));
            }
        }

        Ok(())
    }

    async fn send_modal(&mut self, response: CreateModal) -> Result<()> {
        self.sent_response = true;

        match &self.interaction {
            Interaction::Command(command) => {
                command
                    .create_response(&self.http, CreateInteractionResponse::Modal(response))
                    .await
                    .map(|_| ())?;
            }
            Interaction::Component(component) => {
                component
                    .create_response(&self.http, CreateInteractionResponse::Modal(response))
                    .await
                    .map(|_| ())?;
            }
            Interaction::Modal(modal) => {
                modal
                    .create_response(&self.http, CreateInteractionResponse::Modal(response))
                    .await
                    .map(|_| ())?;
            }
            _ => {
                return Err(anyhow!("Invalid interaction type"));
            }
        }

        Ok(())
    }

    async fn send_acknowledge(&mut self) -> Result<()> {
        self.sent_response = true;

        match &self.interaction {
            Interaction::Command(command) => {
                command
                    .create_response(&self.http, CreateInteractionResponse::Acknowledge)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Component(component) => {
                component
                    .create_response(&self.http, CreateInteractionResponse::Acknowledge)
                    .await
                    .map(|_| ())?;
            }
            Interaction::Modal(modal) => {
                modal
                    .create_response(&self.http, CreateInteractionResponse::Acknowledge)
                    .await
                    .map(|_| ())?;
            }
            _ => {
                return Err(anyhow!("Invalid interaction type"));
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn defer(&mut self) -> Result<()> {
        self.sent_response = true;

        debug!("Deferring interaction");

        match &self.interaction {
            Interaction::Command(command) => {
                command.defer(&self.http).await?;
            }
            Interaction::Component(component) => {
                component.defer(&self.http).await?;
            }
            Interaction::Modal(modal) => {
                modal.defer(&self.http).await?;
            }
            _ => {
                return Err(anyhow!("Invalid interaction type"));
            }
        }

        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn send_response(&mut self, response: InteractionResponse) -> Result<()> {
        debug!("Sending response: {:?}", response);
        match response {
            InteractionResponse::Message(message) => {
                if self.sent_response {
                    self.send_followup(message).await?;
                } else {
                    self.send_message(message).await?;
                }
            }

            InteractionResponse::Modal(modal) => {
                if self.sent_response {
                    return Err(anyhow!("Tried to send a modal after a response"));
                } else {
                    self.send_modal(modal).await?;
                }
            }

            InteractionResponse::None => {
                if !self.sent_response {
                    self.send_acknowledge().await?;
                }
            }
        }

        debug!("Sent response");
        Ok(())
    }
}
