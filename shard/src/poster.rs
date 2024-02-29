use std::{collections::HashMap, sync::Arc};

use log::{debug, warn};
use serde_json::json;
use tokio::{sync::{mpsc::Receiver, RwLock}, time::Instant};
use serenity::{all::{ButtonStyle, UserId}, builder::{CreateActionRow, CreateButton, CreateMessage}, model::id::ChannelId, prelude::TypeMap};
use tokio::time::{Duration, sleep};

use crate::types::ConfigStruct;

use super::{get_subreddit, get_subreddit_search};

pub struct PostRequest {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    // Interval between posts in seconds
    pub interval: Duration,
    pub limit: u32,
    pub current: u32,
    pub last_post: Instant,
    pub author: UserId,
}

pub enum AutoPostCommand {
    Start(PostRequest),
    Stop(ChannelId),
}


// TODO - Never panic!
pub async fn start_loop(mut rx: Receiver<AutoPostCommand>, data: Arc<RwLock<TypeMap>>, http: Arc<serenity::http::Http>) {
    let mut requests = HashMap::new();

    loop {
        sleep(Duration::from_secs(1)).await;

        // Insert any available requests into the hashmap
        while match rx.try_recv() {
            Ok(request) => {
                match request {
                    AutoPostCommand::Start(req) => {
                        requests.insert(req.channel, req);
                    }
                    AutoPostCommand::Stop(channel) => {
                        requests.remove(&channel);
                    }
                }
                true
            }
            Err(_) => false,
        } {}

        let mut to_remove: Vec<ChannelId> = Vec::new();

        // Iterate over the requests and post if the interval has passed
        for (channel, request) in requests.iter_mut() {
            if request.last_post + request.interval < Instant::now() {
                // Post the request
                let data = data.read().await;
                let conf = data.get::<ConfigStruct>().unwrap();
                let mut con = conf.redis.clone();
                let subreddit = request.subreddit.clone();
                let search = request.search.clone();
                let channel = channel.clone();

                request.current += 1;
                if request.current > request.limit {
                    to_remove.push(channel.clone());
                    continue;
                }

                let post = if let Some(search) = search {
                    get_subreddit_search(subreddit, search, &mut con, channel, None).await
                } else {
                    get_subreddit(subreddit, &mut con, channel, None).await
                }.unwrap();

                let mut resp = CreateMessage::default();
                if let Some(em) = post.embed {
                    resp = resp.embed(em);
                }

                if let Some(content) = post.content {
                    resp = resp.content(content);
                }

                if let Some(attachment) = post.file {
                    resp = resp.add_file(attachment);
                }

                resp = resp.components(vec![
                    CreateActionRow::Buttons(vec![
                        CreateButton::new(json!({
                            "command": "cancel_autopost",
                        }).to_string())
                            .label("Stop")
                            .style(ButtonStyle::Danger)
                            ])]);

                match channel.send_message(http.clone(),  resp).await {
                    Ok(_) => {}
                    Err(e) => {
                        debug!("Error sending message: {:?}", e);
                        let dm = match request.author.create_dm_channel(http.clone()).await {
                            Ok(dm) => dm,
                            Err(e) => {
                                warn!("Error creating DM: {:?}", e);
                                continue;
                            }
                        };

                        let msg = CreateMessage::new()
                            .content(format!("Hello there! I tried to post to <#{}> but I don't have permission to do so. Please make sure I have the correct permissions and try again.", channel.get()));
                        match dm.send_message(http.clone(), msg).await {
                            Ok(_) => {}
                            Err(e) => {
                                warn!("Error sending DM: {:?}", e);
                            }
                        };

                        to_remove.push(channel.clone());
                    }
                
                };
                
                // Update the last post time
                request.last_post = Instant::now();
            }
        }

        for channel in to_remove {
            requests.remove(&channel);
        }
    }
}
