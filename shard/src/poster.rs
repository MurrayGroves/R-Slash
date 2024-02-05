use std::{collections::HashMap, sync::Arc};

use connection_pooler::ResourceManager;
use serde_json::json;
use tokio::sync::{mpsc::Receiver, RwLock};
use serenity::{all::ButtonStyle, builder::{CreateActionRow, CreateButton, CreateMessage}, model::id::ChannelId, prelude::TypeMap};
use tokio::time::{Duration, sleep};

use super::{get_subreddit, get_subreddit_search};

pub struct PostRequest {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    // Interval between posts in seconds
    pub interval: u16,
    pub limit: u32,
    pub current: u32,
    pub last_post: u64,
}

pub enum AutoPostCommand {
    Start(PostRequest),
    Stop(ChannelId),
}

pub async fn start_loop(rx: &mut Receiver<AutoPostCommand>, data: Arc<RwLock<TypeMap>>, http: Arc<serenity::http::Http>) {
    let mut requests = HashMap::new();

    loop {
        sleep(Duration::from_secs(5)).await;

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
            if request.last_post + (request.interval as u64) < std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() {
                // Post the request
                let data = data.read().await;
                let client = data.get::<ResourceManager<redis::aio::Connection>>().unwrap();
                let redis_client = client.get_available_resource().await;
                let mut con = redis_client.lock().await;
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

                channel.send_message(http.clone(),  resp).await.unwrap();
                
                // Update the last post time
                request.last_post = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            }
        }

        for channel in to_remove {
            requests.remove(&channel);
        }
    }
}
