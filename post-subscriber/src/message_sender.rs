use std::{sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{Instant, timeout},
};

use serenity::all::Http;

use post_subscriber::{Bot, Subscription};

use metrics::{counter, histogram};
use tracing::{debug, error, info};
use user_config_manager::{TextAllowLevel, get_channel_config};

pub(crate) struct Alert {
    pub(crate) post: post_api::PostInContext,
    pub(crate) timestamp: Instant,
    pub(crate) text_allow_level: TextAllowLevel,
}

pub(crate) enum Command {
    SetAlert(Alert),
    SendToSubcription(Arc<Subscription>),
}

pub(crate) struct MessageSender<'a> {
    pub(crate) discord_bb: Arc<Http>,
    pub(crate) discord_rs: Arc<Http>,
    pub(crate) mongodb: mongodb::Client,
    pub(crate) failed_req_counter: Arc<Mutex<usize>>,
    pub(crate) interval: tokio::time::Interval,
    pub(crate) channel: tokio::sync::mpsc::Receiver<Command>,
    pub(crate) message: Option<serenity::builder::CreateMessage<'a>>,
    pub(crate) text_allow_level: TextAllowLevel,
    pub(crate) alert_timestamp: Instant,
    pub(crate) deletion_request_channel: tokio::sync::mpsc::Sender<Arc<Subscription>>,
}

impl<'a> MessageSender<'a> {
    pub(crate) async fn sender_loop(mut self) {
        loop {
            let command = self
                .channel
                .recv()
                .await
                .expect("Sender loop's channel was closed");

            match command {
                Command::SetAlert(alert) => {
                    self.message = Some(alert.post.buttonless_message());
                    self.alert_timestamp = alert.timestamp;
                    self.text_allow_level = alert.text_allow_level;
                }
                Command::SendToSubcription(subscription) => self.send_message(subscription).await,
            }
        }
    }

    async fn send_message(&mut self, subscription: Arc<Subscription>) {
        let text_allow_level =
            match get_channel_config(&mut self.mongodb.clone(), subscription.channel).await {
                Ok(x) => x.text_allowed.unwrap_or_default(),
                Err(e) => {
                    info!("Error getting channel config: {:?}", e);
                    error!("Error getting channel config");
                    return;
                }
            };

        if !text_allow_level.allows_for(self.text_allow_level) {
            debug!(
                "Post with {:?} doesn't match text allow level of {:?}",
                self.text_allow_level, text_allow_level
            );
            return;
        }

        self.interval.tick().await;

        let http = match subscription.bot {
            Bot::BB => &self.discord_bb,
            Bot::RS => &self.discord_rs,
        };
        let response = match timeout(
            Duration::from_secs(30),
            subscription
                .channel
                .send_message(http, self.message.as_ref().map(|x| x.clone()).unwrap()),
        )
        .await
        {
            Ok(resp) => resp,
            Err(_) => {
                error!("Timeout while sending message to channel");
                let mut counter = self.failed_req_counter.lock().await;
                *counter += 1;
                counter!("subscriber_failed_messages").increment(1);
                counter!("subscriber_failed_message_timeouts").increment(1);
                return;
            }
        };

        match response {
            Ok(_) => {
                let elapsed = self.alert_timestamp.elapsed();
                debug!(
                    "Sent alert to channel {}, was added {} seconds ago",
                    subscription.channel,
                    elapsed.as_secs()
                );
                histogram!("subscriber_notification_latency").record(elapsed.as_millis() as f64);
                counter!("subscriber_sent_messages").increment(1);
            }
            Err(e) => {
                let error_text = format!("Failed to send message: {:?}", e);
                if let serenity::Error::Http(e) = e
                    && let serenity::http::HttpError::UnsuccessfulRequest(e) = e
                {
                    if e.error.code.0 == 10003 {
                        debug!("Channel doesn't exist anymore, marking subscription for deletion.");
                        self.deletion_request_channel
                            .send(subscription)
                            .await
                            .expect("Couldn't send deletion request because channel was closed");
                    } else {
                        error!("Failed to send message to channel: {:?}", e);
                        let mut failed_reqs = self.failed_req_counter.lock().await;
                        *failed_reqs += 1;
                        counter!("subscriber_failed_messages").increment(1);
                    }
                } else {
                    error!("{}", error_text);
                    let mut failed_reqs = self.failed_req_counter.lock().await;
                    *failed_reqs += 1;
                    counter!("subscriber_failed_messages").increment(1);
                }
            }
        }
    }
}
