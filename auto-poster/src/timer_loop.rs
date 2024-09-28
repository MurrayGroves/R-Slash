use std::{ops::Deref, time::Duration};

use auto_poster::PostMemory;
use mongodb::bson::doc;
use post_api::queue_subreddit;
use serenity::all::CreateMessage;
use tokio::{select, time::Instant};
use tracing::{debug, error, warn};

use crate::AutoPostServer;

pub async fn timer_loop(
    server: AutoPostServer,
    mut new_memory_alert: tokio::sync::mpsc::Receiver<()>,
) {
    loop {
        let to_sleep = {
            let autoposts = server.autoposts.read().await;
            if let Some(memory) = autoposts.queue.peek() {
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
                    drop(autoposts);

                    debug!("Posting autopost: {:?}", autopost);

                    let channel = autopost.channel.clone();
                    let bot = autopost.bot.clone();

                    if let Some(limit) = autopost.limit {
                        let autoposts = server.autoposts.write().await;
                        (autopost.deref().get_mut()).current += 1;
                        drop(autoposts);
                        if autopost.current < limit {
                            let mut autoposts = server.autoposts.write().await;
                            autopost.get_mut().next_post = autopost.next_post + autopost.interval;
                            autoposts.queue.push(autopost.clone());
                        } else {
                            let mut autoposts = server.autoposts.write().await;
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
                            drop(autoposts);
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
                        let mut autoposts = server.autoposts.write().await;
                        (autopost.deref().get_mut()).current += 1;
                        autopost.get_mut().next_post = autopost.next_post + autopost.interval;
                        autoposts.queue.push(autopost.clone());
                    };

                    let http = server.discords[&bot].clone();
                    let autopost_clone = autopost.clone();
                    let redis = server.redis.clone();

                    let is_custom = !server.default_subs.contains(&autopost.subreddit);
                    tokio::spawn(async move {
                        let mut redis = redis.clone();

                        if is_custom {
                            match queue_subreddit(
                                &autopost_clone.subreddit,
                                &mut redis.clone(),
                                autopost_clone.bot,
                            )
                            .await
                            {
                                Ok(_) => (),
                                Err(e) => {
                                    warn!("Error queueing subreddit: {:?}", e);
                                }
                            };
                        }

                        let message = if let Some(search) = autopost_clone.search.clone() {
                            post_api::get_subreddit_search(
                                autopost_clone.subreddit.clone(),
                                search,
                                &mut redis,
                                autopost_clone.channel,
                            )
                            .await
                        } else {
                            post_api::get_subreddit(
                                autopost_clone.subreddit.clone(),
                                &mut redis,
                                autopost_clone.channel,
                            )
                            .await
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

                            debug!("Sending message: {:?}", resp);
                            let to_return = select! {
                                result = channel.send_message(http, resp) => result,
                                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                                    Err(serenity::Error::Other("Sending message took more than 10 seconds"))
                                }
                            };
                            debug!("Sent message");
                            to_return
                        } {
                            warn!("Error sending message: {:?}", why);
                        }
                    });

                    let autoposts = server.autoposts.read().await;
                    if let Some(next) = autoposts.queue.peek() {
                        debug!("Memories: {:?}", autoposts.queue);
                        debug!("Next post: {:?}", next);
                        debug!("Next post in: {:?}", next.next_post - Instant::now());
                        debug!("Now: {:?}", Instant::now());
                        next.next_post - Instant::now()
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
            }
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