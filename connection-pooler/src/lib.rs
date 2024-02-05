use serenity::futures::{stream, StreamExt};
use tracing::{warn, info, trace, debug};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct LockedResource<T: Send + Sync> {
    last_accessed: Arc<Mutex<Instant>>,
    data: Arc<Mutex<T>>,
}

impl<T: Send + Sync> LockedResource<T> {
    fn new(contents: T) -> Self
    {
        Self {
            last_accessed: Arc::new(Mutex::new(Instant::now())),
            data: Arc::new(Mutex::new(contents)),
        }
    }
}

#[derive(Clone)]
pub struct ResourceManager<T: Send + Sync > {
    resources: Arc<Mutex<Vec<Arc<LockedResource<T>>>>>,
    maker: Arc<dyn Fn() -> Arc<Mutex<Pin<Box<dyn Future<Output = T> + Send>>>> + Send + Sync>,
}


impl <T> ResourceManager<T> where T: Send + Sync + 'static, Self: Sized {
    pub fn clone(&self) -> Self {
        Self {
            resources: self.resources.clone(),
            maker: self.maker.clone(),
        }
    }

    pub async fn new(maker: impl Fn() -> Arc<Mutex<Pin<Box<dyn Future<Output = T> + Send>>>> + Send + Sync + 'static) -> Self {
        let new = Self {
            resources: Arc::new(Mutex::new(Vec::new())),
            maker: Arc::new(maker),
        };

        // Spawn a background thread for cleanup
        let resource_manager = new.clone();
        tokio::spawn(async move {
            loop {
                thread::sleep(Duration::from_secs(60)); // Adjust the interval as needed

                // Remove locks that have been held for too long
                resource_manager.remove_unavailable().await;
            }
        });

        new
    }

    pub async fn get_available_resource(&self) -> Arc<Mutex<T>> {
        self.remove_unavailable().await;

        let resources = self.resources.lock().await;
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
                let new = Arc::<serenity::prelude::Mutex<std::pin::Pin<Box<(dyn std::future::Future<Output = T> + std::marker::Send + 'static)>>>>::try_unwrap(self.maker.clone()()).unwrap_or_else(|_| panic!("Failed to unwrap maker")).into_inner();
                let resource: Arc<LockedResource<T>> = Arc::new(LockedResource::new(new.await));
                self.add(resource.clone()).await;
                resource
            }
        };

        *available.last_accessed.lock().await = Instant::now();

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