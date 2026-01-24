use crate::AutoPostServer;
use auto_poster::{check_ordered, MemoryRef, PostMemory};
use metrics::{counter, gauge, histogram};
use mongodb::bson::doc;
use post_api::{optional_post_to_message, queue_subreddit, PostApiError};
use reddit_proxy::RedditProxyClient;
use rslash_common::SubredditStatus;
use serenity::all::{ChannelId, CreateMessage};
use std::{ops::Deref, sync::Arc, time::Duration};
use tarpc::context::Context;
use tokio::{select, time::Instant};
use tracing::{debug, error, warn};
use user_config_manager::get_channel_config;

async fn delete_auto_post(server: &AutoPostServer, autopost: Arc<MemoryRef>) {
    let coll: mongodb::Collection<PostMemory> = server.db.database("state").collection("autoposts");
    counter!("autoposter_deleted_autoposts_by_system").increment(1);

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

async fn post_error_to_message(
    reddit_proxy: &RedditProxyClient,
    e: anyhow::Error,
    subreddit: &str,
) -> CreateMessage<'static> {
    let validity = reddit_proxy
        .check_subreddit_valid(Context::current(), subreddit.to_string())
        .await
        .unwrap_or_else(|e| {
            warn!("Error checking subreddit validity: {}", e);
            Ok(SubredditStatus::Valid)
        })
        .unwrap_or_else(|e| {
            warn!("Error checking subreddit validity: {}", e);
            SubredditStatus::Valid
        });

    match validity {
        SubredditStatus::Valid => {
            if let Some(x) = e.downcast_ref::<PostApiError>() {
                match x {
                    PostApiError::NoPostsFound { subreddit } => {
                        rslash_common::error_message(
                            "No posts found".to_string(),
                            format!("Deleted autopost for r/{} because no supported posts were found. For example, they might all be text posts.", subreddit)
                        )
                    }
                    _ => {
                        error!("Error getting subreddit for auto-post: {:?}", e);
                        rslash_common::error_message(
                            "Unexpected Error".to_string(),
                            format!("Deleted autopost for r/{} due to an unexpected error.", subreddit)
                        )
                    }
                }
            } else {
                error!("Error getting subreddit for auto-post: {:?}", e);
                rslash_common::error_message(
                    "Unexpected Error".to_string(),
                    format!(
                        "Deleted autopost for r/{} due to an unexpected error.",
                        subreddit
                    ),
                )
            }
        }
        SubredditStatus::Invalid(reason) => {
            debug!("Subreddit response not 200: {}", reason);
            rslash_common::error_message(
                "Subreddit Inaccessible".to_string(),
                format!("Deleted autopost for r/{} because it's either private, does not exist, or has been banned.", subreddit)
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
<<<<<<< ours
                if memory.next() <= now {
                    debug!("Running {:?} behind", now - memory.next());
                    histogram!("autoposter_post_delay").record(now - memory.next());
                    drop(autoposts);

                    // Used to ensure loop waits until task has popped from queue before continuing
                    let (tx, rx) = tokio::sync::oneshot::channel();
||||||| ancestor
                if memory.next_post <= now {
                    debug!("Running {:?} behind", now - memory.next_post);
                    histogram!("autoposter_post_delay").record(now - memory.next_post);

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
                    counter!("autoposter_attempted_posts").increment(1);

                    let channel = autopost.channel.clone();

                    let bot = autopost.bot.clone();

                    let http = server.discords[&bot].clone();
                    let autopost_clone = autopost.clone();
                    let redis = server.redis.clone();
=======
                if memory.next() <= now {
                    debug!("Running {:?} behind", now - memory.next());
                    histogram!("autoposter_post_delay").record(now - memory.next());
                    drop(autoposts);

                    // Used to ensure loop waits until task has popped from queue before continuing
                    let (tx, rx) = tokio::sync::oneshot::channel();
>>>>>>> theirs
                    let server_clone = server.clone();
                    // Spawn task to post the autopost
                    tokio::spawn(async move {
                        let mut server = server_clone;

                        // Get next autopost from queue, removing it from the queue
                        let mut autoposts = server.autoposts.write().await;
                        let autopost: Arc<MemoryRef> = match autoposts.queue.pop() {
                            Some(x) => x,
                            None => {
                                if let Err(_) = tx.send(()) {
                                    error!("Timer loop receiver dropped!");
                                };
                                return;
                            }
                        };

                        if let Err(_) = tx.send(()) {
                            error!("Timer loop receiver dropped!");
                        };

                        let autopost_guard = autopost.lock().await;
                        drop(autoposts);
                        debug!("Posting autopost: {:?}", autopost);
                        counter!("autoposter_attempted_posts").increment(1);

                        let channel = autopost.channel.clone();

                        let bot = autopost.bot.clone();

                        let http = server.discords[&bot].clone();

                        let is_custom = !server.default_subs.contains(&autopost.subreddit);

                        // Queue subreddit for downloading if it's a custom subreddit
                        if is_custom {
<<<<<<< ours
                            if let Err(e) = async {
                                let text_allow_level = get_channel_config(
                                    &mut server.db,
                                    ChannelId::new(channel.get()),
                                )
                                .await?
                                .text_allowed
                                .unwrap_or_default();

                                queue_subreddit(
                                    &autopost.subreddit,
                                    &mut server.redis,
                                    autopost.bot,
                                    text_allow_level,
                                )
                                .await
                            }
||||||| ancestor
                            match queue_subreddit(
                                &autopost_clone.subreddit,
                                &mut redis.clone(),
                                autopost_clone.bot,
                            )
=======
                            match queue_subreddit(
                                &autopost.subreddit,
                                &mut server.redis,
                                autopost.bot,
                            )
>>>>>>> theirs
                            .await
                            {
                                warn!("Error queueing subreddit: {:?}", e);
                            };
                        }

                        // Get subreddit post
                        let message: Result<CreateMessage, anyhow::Error> =
                            if let Some(search) = autopost.search.clone() {
                                post_api::get_subreddit_search(
                                    &autopost.subreddit,
                                    &search,
                                    &mut server.redis,
                                    autopost.channel.widen(),
                                )
                                .await
                                .map(|post| optional_post_to_message(post, false))
                            } else {
                                post_api::get_subreddit(
                                    &autopost.subreddit,
                                    &mut server.redis,
                                    &mut server.db,
                                    autopost.channel.widen(),
                                    Some(0),
                                )
                                .await
                                .map(|post| post.into())
                            };

                        // If an error occurred getting the post, delete the autopost, and prepare an error message
                        let mut failed = false;
                        let message = match message {
                            Ok(x) => x,
                            Err(e) => {
                                warn!("Error getting subreddit for autopost: {}", e);
                                let subreddit = autopost.subreddit.clone();
                                failed = true;

                                post_error_to_message(&server.reddit_proxy, e, &subreddit).await
                            }
                        };

                        debug!("Sending message: {:?} for autopost {:?}", message, autopost);
                        let message_send_result =
                            channel.widen().send_message(&*http, message).await;
                        debug!("Sent message");

                        // Handle any errors sending the message
                        if let Err(why) = message_send_result {
                            // If we failed because we don't have permission to send to the channel, delete the autopost
                            if let serenity::Error::Http(e) = &why {
                                if let serenity::http::HttpError::UnsuccessfulRequest(e) = e {
                                    // Unknown channel (we got removed from server)
                                    if e.error.code.0 == 10003 {
                                        counter!("autoposter_missing_channel").increment(1);
                                        failed = true;
                                    }
                                }
                            } else {
                                warn!("Error sending message: {:?}", why);
                                counter!("autoposter_unknown_sending_error").increment(1);
                            }
                        } else {
                            counter!("autoposter_sent_messages").increment(1);
                        }

                        // If we succeeded in sending the message, or it failed due to a temporary error
                        if !failed {
                            autopost.increment(&autopost_guard);

                            // Delete autopost if exceeded limit
                            if let Some(limit) = autopost.limit {
                                if autopost.current() >= limit {
                                    drop(autopost_guard);
                                    delete_auto_post(&server, autopost).await;
                                    return;
                                }
                            }

                            // Add back to queue
                            let next = autopost.next();
                            debug!(
                                "Next occurrence of current post {:?} is in {:?}",
                                autopost,
                                next - Instant::now()
                            );

                            let mut autoposts = server.autoposts.write().await;
                            drop(autopost_guard);
                            autoposts.queue.push(autopost);
                            drop(autoposts);

                            debug!("Re-added autopost to queue");

                            let waiting_until = server.waiting_until.read().await;
                            if next < *waiting_until {
                                debug!("Next post is sooner than waiting until, updating waiting until");
                                drop(waiting_until);
                                *server.waiting_until.write().await = next;
                                server.sender.send(()).await.unwrap();
                            }
                        } else {
                            drop(autopost_guard); // Must be dropped, or deletion won't be able to get it!
                            delete_auto_post(&server, autopost).await;
                        }
                    });

                    if let Err(_) = rx.await {
                        error!("Timer loop task sender dropped!");
                    };

                    // Calculate time to sleep to next post
                    let autoposts = server.autoposts.read().await;
                    if let Some(next) = autoposts.queue.peek() {
                        check_ordered(&autoposts.queue);
                        debug!("Next post: {:?}", next);
                        debug!("Next post in: {:?}", next.next() - Instant::now());
                        debug!("Now: {:?}", Instant::now());
                        let mut waiting_until = server.waiting_until.write().await;
                        *waiting_until = next.next();
                        next.next() - Instant::now()
                    } else {
                        let mut waiting_until = server.waiting_until.write().await;
                        *waiting_until = Instant::now() + Duration::from_secs(3600);
                        Duration::from_secs(3600)
                    }
                } else {
                    // We woke up too early and need to sleep more
                    counter!("autoposter_early_wakeups").increment(1);
                    let to_return = memory.next() - now;
                    debug!(
                        "Woke up {:?} too early, it's now {:?} and next is {:?}",
                        to_return,
                        now,
                        memory.next()
                    );
                    check_ordered(&autoposts.queue);
                    histogram!("autoposter_early_wakeup_secs").record(to_return.as_secs_f64());
                    let mut waiting_until = server.waiting_until.write().await;
                    *waiting_until = memory.next();
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
