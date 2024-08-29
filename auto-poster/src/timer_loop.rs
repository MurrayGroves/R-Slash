use std::{ops::Deref, time::Duration};

use auto_poster::PostMemory;
use mongodb::bson::doc;
use serenity::all::CreateMessage;
use tokio::{select, time::Instant};
use tracing::{debug, error, warn};

use crate::AutoPostServer;

pub async fn timer_loop(
    mut server: AutoPostServer,
    mut new_memory_alert: tokio::sync::mpsc::Receiver<()>,
) {
    loop {
        let autoposts = server.autoposts.read().await;
        let to_sleep = if let Some(memory) = autoposts.queue.peek() {
            let now = Instant::now();
            if memory.next_post <= now {
                drop(autoposts);
                let mut autoposts = server.autoposts.write().await;
                let autopost = match autoposts.queue.pop() {
                    Some(x) => x,
                    None => {
                        continue;
                    }
                };

                debug!("Posting autopost: {:?}", autopost);

                let message = if let Some(search) = autopost.search.clone() {
                    post_api::get_subreddit_search(
                        autopost.subreddit.clone(),
                        search,
                        &mut server.redis,
                        autopost.channel,
                        None,
                    )
                    .await
                } else {
                    post_api::get_subreddit(
                        autopost.subreddit.clone(),
                        &mut server.redis,
                        autopost.channel,
                        None,
                    )
                    .await
                };

                let channel = autopost.channel.clone();
                let bot = autopost.bot.clone();

                (autopost.deref().get_mut()).current += 1;
                if let Some(limit) = autopost.limit {
                    if autopost.current < limit {
                        autopost.get_mut().next_post = autopost.next_post + autopost.interval;
                        autoposts.queue.push(autopost);
                    } else {
                        autoposts.by_id.remove(&autopost.id);
                        match autoposts.by_channel.get_mut(&autopost.channel) {
                            Some(x) => {
                                x.remove(&*autopost);
                                if x.len() == 0 {
                                    autoposts.by_channel.remove(&autopost.channel);
                                };
                            }
                            None => {
                                error!("Tried to get channel to remove autopost from it but channel didn't exist");
                            }
                        };
                        let client = server.db.lock().await;
                        let coll: mongodb::Collection<PostMemory> =
                            client.database("state").collection("autoposts");

                        let filter = doc! {
                            "id": autopost.id as i64,
                        };

                        match coll.delete_one(filter).await {
                            Ok(_) => (),
                            Err(e) => {
                                error!("Failed to delete autopost from database: {:?}", e);
                            }
                        };
                    }
                } else {
                    autopost.get_mut().next_post = autopost.next_post + autopost.interval;
                    autoposts.queue.push(autopost);
                };

                let message = match match message {
                    Ok(x) => x,
                    Err(e) => {
                        warn!("Error getting subreddit for autopost: {}", e);
                        rslash_types::InteractionResponse::Message(
                            rslash_types::InteractionResponseMessage {
                                content: Some(
                                    "Error getting subreddit for auto-post, please report in the support server."
                                        .to_string(),
                                ),
                                ephemeral: true,
                                ..Default::default()
                            },
                        )
                    }
                } {
                    rslash_types::InteractionResponse::Message(message) => message,
                    _ => {
                        warn!("Invalid response from post_api");
                        rslash_types::InteractionResponseMessage {
                                content: Some(
                                    "Error getting subreddit for auto-post (invalid post_api enum), please report in the support server."
                                        .to_string(),
                                ),
                                ephemeral: true,
                                ..Default::default()
                            }
                    }
                };

                if let Err(why) = {
                    let mut resp = CreateMessage::new();
                    if let Some(embed) = message.embed.clone() {
                        resp = resp.embed(embed);
                    };

                    if let Some(content) = message.content.clone() {
                        resp = resp.content(content);
                    };

                    channel.send_message(&server.discords[&bot], resp).await
                } {
                    warn!("Error sending message: {:?}", why);
                }

                if let Some(next) = autoposts.queue.peek() {
                    next.next_post - now
                } else {
                    Duration::from_secs(3600)
                }
            } else {
                let to_return = memory.next_post - now;
                drop(autoposts);
                to_return
            }
        } else {
            drop(autoposts);
            Duration::from_secs(3600)
        };

        // Sleep until next post needs to be sent or a new post is added that's sooner than the next post
        select! {
            _ = tokio::time::sleep(to_sleep) => {},
            recv = new_memory_alert.recv() => {
                if recv.is_none() {
                    panic!("Failed to receive new memory alert, channel is closed!!!!");
                }
            }
        }
    }
}
