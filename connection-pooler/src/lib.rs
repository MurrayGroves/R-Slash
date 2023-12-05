use serenity::futures::{stream, StreamExt};
use tracing::{warn, info};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct LockedResource<T> {
    last_accessed: Arc<Mutex<Instant>>,
    data: Arc<Mutex<T>>,
}

impl<T> LockedResource<T> {
    async fn new(maker: Arc<Box<dyn Fn() -> T + Send + Sync>>) -> Self

    {
        Self {
            last_accessed: Arc::new(Mutex::new(Instant::now())),
            data: Arc::new(Mutex::new(maker())),
        }
    }
}

#[derive(Clone)]
pub struct ResourceManager<T: Send + Sync > {
    resources: Arc<Mutex<Vec<Arc<LockedResource<T>>>>>,
    maker: Arc<Box<dyn Fn() -> T + Send + Sync>>,
}


impl <T> ResourceManager<T> where T: Send + Sync + 'static, Self: Sized {
    pub fn clone(&self) -> Self {
        Self {
            resources: self.resources.clone(),
            maker: self.maker.clone(),
        }
    }

    pub fn new(maker: Arc<Box<dyn Fn() -> T + Send + Sync + 'static>>) -> Self {
        let new = Self {
            resources: Arc::new(Mutex::new(Vec::new())),
            maker,
        };

        // Spawn a background thread for cleanup
        let resource_manager = new.clone();
        thread::spawn(move || async move {
            loop {
                thread::sleep(Duration::from_secs(60)); // Adjust the interval as needed

                // Remove locks that have been held for too long
                resource_manager.remove_unavailable().await;
            }
        });

        new
    }

    pub async fn get_available_resource(&self) -> Arc<Mutex<T>> {
        println!("Getting available resource");
        self.remove_unavailable().await;

        let resources = self.resources.lock().await;
        println!("Locked resources");
        let mut available = Option::None;
        for resource in &*resources {
            match resource.data.try_lock() {
                Ok(_) => {
                    available = Some(Arc::clone(resource));
                    break;
                },
                Err(_) => {

                }
            }
        }
        drop(resources);

        let available = match available {
            Some(resource) => resource,
            None => {
                println!("Creating new resource");
                let resource: Arc<LockedResource<T>> = Arc::new(LockedResource::new(self.maker.clone()).await);
                println!("Adding resource");
                self.add(resource.clone()).await;
                println!("Added resource");
                resource
            }
        };

        *available.last_accessed.lock().await = Instant::now();

        println!("Returning available resource");
        available.data.clone()

    }

    // Remove one unavailable resource if it has been unavailable for more than 2 minutes
    async fn remove_unavailable(&self) {
        let mut resources_lock: tokio::sync::MutexGuard<'_, Vec<Arc<LockedResource<T>>>> = self.resources.lock().await;

        let mut to_remove = None;
        for (i, resource) in resources_lock.iter().enumerate() {
            let _ = resource.data.try_lock();
            let locked = resource.data.try_lock().is_err();
            if resource.last_accessed.lock().await.elapsed() > Duration::from_secs(120)  && locked{
                warn!("Removing unavailable resource");
                to_remove = Some(i);
                break
            }
        }
        match to_remove {
            None => return,
            Some(to_remove) => {
                let resources = &mut *resources_lock;
                resources.swap_remove(to_remove);
            },
        }
    }

    async fn add(&self, resource: Arc<LockedResource<T>>) {
        let mut resources = self.resources.lock().await;
        resources.push(resource);
    }

}

impl <T: 'static + Send + Sync> serenity::prelude::TypeMapKey for ResourceManager<T> {
    type Value = ResourceManager<T>;
}