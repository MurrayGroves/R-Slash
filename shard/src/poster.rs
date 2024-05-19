use std::{collections::HashMap, sync::Arc};

use futures::TryStreamExt;
use log::{debug, warn};
use mongodb::{bson::{doc, Document}, options::ClientOptions};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{sync::{mpsc::Receiver, RwLock}, time::Instant};
use serenity::{all::{ButtonStyle, UserId}, builder::{CreateActionRow, CreateButton, CreateMessage}, model::id::ChannelId, prelude::TypeMap};
use tokio::time::{Duration, sleep};

use crate::types::ConfigStruct;

use super::{get_subreddit, get_subreddit_search};

#[derive(Debug, Clone)]
pub struct PostRequest {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    // Interval between posts in seconds
    pub interval: Duration,
    pub limit: Option<u32>,
    pub current: u32,
    pub last_post: Instant,
    pub author: UserId,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct PostMemory {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    pub interval: Duration,
    pub limit: Option<u32>,
    pub current: u32,
    pub author: UserId,
    pub shard_id: u64,
}

pub enum AutoPostCommand {
    Start(PostRequest),
    Stop(ChannelId),
}


// TODO - Never panic!
pub async fn start_loop(mut rx: Receiver<AutoPostCommand>, data: Arc<RwLock<TypeMap>>, http: Arc<serenity::http::Http>, shard_id: u64) {
    debug!("Starting autopost loop for shard {}", shard_id);
    let mut requests = HashMap::new();

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Shard {} autoposter", shard_id));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("state");
    let coll = db.collection::<PostMemory>("autopost");

    // Load all the post memories from the database
    let mut cursor = coll.find(None, None).await.unwrap();
    while let Some(post) = cursor.try_next().await.unwrap() {
        if post.shard_id != shard_id {
            continue;
        }
        
        debug!("Loaded post memory: {:?}", post);
        requests.insert(post.channel, PostRequest {
            channel: post.channel,
            subreddit: post.subreddit,
            search: post.search,
            interval: post.interval,
            limit: post.limit,
            current: post.current,
            last_post: Instant::now(),
            author: post.author,
        });
    };

    loop {
        sleep(Duration::from_secs(1)).await;

        // Insert any available requests into the hashmap
        while match rx.try_recv() {
            Ok(request) => {
                match request {
                    AutoPostCommand::Start(req) => {
                        requests.insert(req.channel.clone(), req.clone());
                        let memory = PostMemory {
                            channel: req.channel,
                            subreddit: req.subreddit,
                            search: req.search,
                            interval: req.interval,
                            limit: req.limit,
                            current: req.current,
                            author: req.author,
                            shard_id,
                        };
                        coll.insert_one(memory, None).await.unwrap();
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
                if request.limit.is_some() && request.current > request.limit.unwrap() {
                    to_remove.push(channel.clone());
                    continue;
                }

                // Update the post memory
                let filter: Document = doc! {"channel": request.channel.get().to_string()};
                let mut post_memory = match coll.find_one(filter.clone(), None).await {
                    Ok(Some(post)) => post,
                    Ok(None) => {
                        debug!("Post memory not found for channel {}", request.channel.get());
                        continue;
                    }
                    Err(e) => {
                        warn!("Error finding post memory: {:?}", e);
                        continue;
                    }
                };

                post_memory.current = request.current;

                match coll.replace_one(filter, post_memory, None).await {
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Error updating post memory: {:?}", e);
                    }
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
            let filter: Document = doc! {"channel": channel.get() as i64};
            match coll.delete_one(filter, None).await {
                Ok(_) => {}
                Err(e) => {
                    warn!("Error deleting post memory: {:?}", e);
                }
            }
        }
    }
}
