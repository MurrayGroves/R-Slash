use std::{ops::Deref, sync::Arc, time::Duration};

use auto_poster::PostMemory;
use mongodb::bson::doc;
use post_api::{queue_subreddit, PostApiError, SubredditStatus};
use rslash_common::{InteractionResponse, InteractionResponseMessage};
use serenity::all::{CreateEmbed, CreateMessage};
use tokio::{select, time::Instant};
use tracing::{debug, error, warn};

use crate::{AutoPostServer, UnsafeMemory};

async fn delete_auto_post(server: &AutoPostServer, autopost: Arc<UnsafeMemory>) {
    let client = server.db.lock().await;
    let coll: mongodb::Collection<PostMemory> = client.database("state").collection("autoposts");

    let filter = doc! {
        "id": autopost.id,
    };

    match coll.delete_one(filter).await {
        Ok(_) => (),
        Err(e) => {
            error!("Failed to delete autopost from database: {:?}", e);
        }
    };

    let mut autoposts = server.autoposts.write().await;
    match autoposts.by_channel.get_mut(&autopost.channel) {
        Some(x) => {
            x.remove(&*autopost);
            if x.len() == 0 {
                autoposts.by_channel.remove(&autopost.channel);
            };
        }
        None => {
            error!(
                "Tried to get channel to remove autopost from it but channel didn't exist, {:?}",
                autopost
            );
        }
    };
    drop(autoposts);
}

async fn post_error_to_message(e: anyhow::Error, subreddit: &str) -> InteractionResponse {
    let validity = post_api::check_subreddit_valid(subreddit)
        .await
        .unwrap_or_else(|e| {
            warn!("Error checking subreddit validity: {}", e);
            SubredditStatus::Valid
        });

    match validity {
        SubredditStatus::Valid => {
            if let Some(x) = e.downcast_ref::<PostApiError>() {
                match x {
					PostApiError::NoPostsFound { subreddit } => {
						rslash_common::error_response(
							"No posts found",
							&format!("Deleted autopost for r/{} because no supported posts were found. For example, they might all be text posts.", subreddit)
						)

					}
					_ => {
						error!("Error getting subreddit for auto-post: {:?}", e);
						rslash_common::error_response(
							"Unexpected Error",
							&format!("Deleted autopost for r/{} due to an unexpected error.", subreddit)
						)
					}
				}
            } else {
                error!("Error getting subreddit for auto-post: {:?}", e);
                rslash_common::error_response(
                    "Unexpected Error",
                    &format!(
                        "Deleted autopost for r/{} due to an unexpected error.",
                        subreddit
                    ),
                )
            }
        }
        SubredditStatus::Invalid(reason) => {
            debug!("Subreddit response not 200: {}", reason);
            rslash_common::error_response(
				"Subreddit Inaccessible",
				&format!("Deleted autopost for r/{} because it's either private, does not exist, or has been banned.", subreddit)
			)
        }
    }
}

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
                    debug!("Running {:?} behind", now - memory.next_post);

                    // Get next autopost from queue, removing it from the queue
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

                    let http = server.discords[&bot].clone();
                    let autopost_clone = autopost.clone();
                    let redis = server.redis.clone();
                    let server_clone = server.clone();

                    let is_custom = !server.default_subs.contains(&autopost.subreddit);
                    // Spawn task to post the autopost
                    tokio::spawn(async move {
                        let mut server = server_clone;

                        // Queue subreddit for downloading if it's a custom subreddit
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

                        // Get subreddit post
                        let message = if let Some(search) = autopost_clone.search.clone() {
                            post_api::get_subreddit_search(
                                autopost_clone.subreddit.clone(),
                                search,
                                &mut server.redis,
                                autopost_clone.channel,
                            )
                            .await
                        } else {
                            post_api::get_subreddit(
                                autopost_clone.subreddit.clone(),
                                &mut server.redis,
                                autopost_clone.channel,
                            )
                            .await
                        };

                        // If an error occurred getting the post, delete the autopost, and prepare an error message
                        let mut failed = false;
                        let message = match message {
                            Ok(x) => x,
                            Err(e) => {
                                warn!("Error getting subreddit for autopost: {}", e);
                                delete_auto_post(&server, autopost_clone.clone()).await;
                                failed = true;

                                post_error_to_message(e, &autopost_clone.subreddit).await
                            }
                        };

                        // If the post API didn't return an error, but returned an invalid response, prepare a new error message
                        let message = if let InteractionResponse::Message(message) = message {
                            message
                        } else {
                            warn!("Invalid response from post_api");
                            InteractionResponseMessage {
									embed: Some(
										CreateEmbed::default()
											.title("Unexpected Error")
											.description("Error getting subreddit for auto-post, please report in the support server. (Invalid response from post_api)")
											.color(0xff0000)
											.to_owned(),
									),
									..Default::default()
								}
                        };

                        // Send message
                        let mut resp = CreateMessage::new();
                        if let Some(embed) = message.embed.clone() {
                            resp = resp.embed(embed);
                        };

                        if let Some(content) = message.content.clone() {
                            resp = resp.content(content);
                        };

                        debug!("Sending message: {:?}", resp);
                        let message_send_result = channel.send_message(http, resp).await;
                        debug!("Sent message");

                        // Handle any errors sending the message
                        if let Err(why) = message_send_result {
                            // If we failed because we don't have permission to send to the channel, delete the autopost
                            if let serenity::Error::Http(e) = &why {
                                if let serenity::http::HttpError::UnsuccessfulRequest(e) = e {
                                    // Unknown channel (we got removed from server)
                                    if e.error.code == 10003 {
                                        delete_auto_post(&server, autopost.clone()).await;
                                        failed = true;
                                    }
                                }
                            } else {
                                warn!("Error sending message: {:?}", why);
                            }
                        }

                        // If we succeeded in sending the message, or it failed due to a temporary error, re-add the autopost to the queue
                        if !failed {
                            // Re-add autopost to queue if it hasn't reached its limit
                            if let Some(limit) = autopost.limit {
                                let autoposts = server.autoposts.write().await;
                                autopost.deref().get_mut().current += 1;
                                drop(autoposts);
                                if autopost.current < limit {
                                    let mut autoposts = server.autoposts.write().await;
                                    autopost.deref().get_mut().next_post =
                                        Instant::now() + autopost.interval;
                                    autoposts.queue.push(autopost.clone());
                                } else {
                                    delete_auto_post(&server, autopost.clone()).await;
                                }
                            } else {
                                let mut autoposts = server.autoposts.write().await;
                                autopost.deref().get_mut().current += 1;
                                autopost.deref().get_mut().next_post =
                                    Instant::now() + autopost.interval;
                                autoposts.queue.push(autopost.clone());
                            };
                        };
                    });

                    // Calculate time to sleep to next post
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
                    // We woke up too early and need to sleep more
                    let to_return = memory.next_post - now;
                    drop(autoposts);
                    to_return
                }
            } else {
                // There's no autoposts in the queue, sleep for an hour (we'll get interrupted when a new one is added)
                drop(autoposts);
                Duration::from_secs(3600)
            }
        };

        debug!("Sleeping for {:?}", to_sleep);

        // Sleep until next post needs to be sent or a new post is added that's sooner than the next post
        select! {
            _ = tokio::time::sleep(to_sleep) => {},
            recv = new_memory_alert.recv() => {
                debug!("Received new memory alert");
                if recv.is_none() {
                    panic!("Failed to receive new memory alert, channel is closed!!!!");
                }
            }
        }
    }
}
