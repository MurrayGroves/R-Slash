#![feature(sync_unsafe_cell)]
#![feature(ptr_as_ref_unchecked)]
#![feature(binary_heap_into_iter_sorted)]

use serde::{Deserialize, Serialize};
use serde_with::serde_as;
use serenity::all::ChannelId;
use std::cell::SyncUnsafeCell;
use std::collections::BinaryHeap;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::{Duration, Instant};
use tracing::{debug, error};

#[tarpc::service]
pub trait AutoPoster {
    async fn register_autopost(
        subreddit: String,
        channel: u64,
        interval: Duration,
        limit: Option<u32>,
        search: Option<String>,
        bot: u64,
        interaction_id: u64,
    ) -> Result<i64, String>;

    async fn delete_autopost(id: i64, channel_id: u64) -> Result<Option<PostMemory>, String>;

    async fn list_autoposts(channel: u64, bot: u64) -> Result<Vec<PostMemory>, String>;

    async fn ping() -> Result<(), String>;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "PostMemoryIntermediate", into = "PostMemoryIntermediate")]
pub struct PostMemory {
    pub channel: ChannelId,
    pub subreddit: String,
    pub search: Option<String>,
    pub interval: Duration,
    pub limit: Option<u32>,
    pub bot: u64,
    pub id: i64,
}

impl From<PostMemoryIntermediate> for PostMemory {
    fn from(value: PostMemoryIntermediate) -> Self {
        Self {
            id: value.id.unwrap_or(value.channel as i64),
            channel: ChannelId::new(value.channel),
            subreddit: value.subreddit,
            search: value.search,
            interval: Duration::from_secs(value.interval.try_into().unwrap()),
            limit: value.limit,
            bot: value.bot.try_into().unwrap(),
        }
    }
}

impl From<PostMemory> for PostMemoryIntermediate {
    fn from(value: PostMemory) -> Self {
        Self {
            channel: value.channel.get(),
            subreddit: value.subreddit,
            search: value.search,
            interval: value.interval.as_secs().try_into().unwrap(),
            limit: value.limit,
            bot: value.bot.try_into().unwrap(),
            id: Some(value.id),
        }
    }
}

#[serde_as]
#[derive(Deserialize, Serialize)]
pub struct PostMemoryIntermediate {
    pub channel: u64,
    pub subreddit: String,
    pub search: Option<String>,
    pub interval: i64,
    pub limit: Option<u32>,
    pub bot: i64,
    pub id: Option<i64>,
}

impl PartialEq for PostMemory {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for PostMemory {}

impl Hash for PostMemory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

pub struct MemoryRef {
    memory: PostMemory,
    next: SyncUnsafeCell<Instant>,
    current: SyncUnsafeCell<u32>,
    /// Must be held while post is being sent or deleted
    lock: Mutex<()>,
}

impl Debug for MemoryRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ID: {}, Current: {}, Next: {:?}",
            self.memory.id,
            self.current(),
            self.next()
        )
    }
}

impl Hash for MemoryRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.memory.id.hash(state);
    }
}

impl Deref for MemoryRef {
    type Target = PostMemory;

    fn deref(&self) -> &Self::Target {
        &self.memory
    }
}

impl PartialEq for MemoryRef {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for MemoryRef {}

impl Ord for MemoryRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        unsafe { self.next().cmp(&other.next()).reverse() }
    }
}

impl PartialOrd for MemoryRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl MemoryRef {
    pub fn into_inner(self) -> PostMemory {
        self.memory
    }

    pub fn clone_inner(&self) -> PostMemory {
        self.memory.clone()
    }

    pub fn next(&self) -> Instant {
        unsafe { self.next.get().as_ref_unchecked().clone() }
    }

    pub fn current(&self) -> u32 {
        unsafe { *self.current.get().as_ref_unchecked() }
    }

    pub fn new(value: PostMemory) -> Self {
        Self {
            next: SyncUnsafeCell::new(Instant::now() + value.interval),
            current: SyncUnsafeCell::new(0),
            memory: value,
            lock: Mutex::new(()),
        }
    }

    pub async fn lock(&self) -> MutexGuard<()> {
        self.lock.lock().await
    }

    /// You must prove you have the lock!
    pub fn increment(&self, _guard: &MutexGuard<()>) {
        unsafe {
            let current = self.current.get();
            *current += 1;

            let next = self.next.get();
            *next = Instant::now() + self.memory.interval;
        }
    }
}

pub fn check_ordered(queue: &BinaryHeap<Arc<MemoryRef>>) {
    let mut last = None;
    for memory in queue.clone().into_iter_sorted() {
        if let Some(last) = last {
            if memory > last {
                error!("Queue not ordered!");
                debug!("Queue not ordered! {:?} is after {:?}", memory, last);
            }
        }
        last = Some(memory);
    }
    debug!("Queue is ordered");
}
