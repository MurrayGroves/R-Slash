use tracing::warn;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct LockedResource<T> {
    last_accessed: Arc<Mutex<Instant>>,
    data: Arc<Mutex<T>>,
}

impl <T> LockedResource<T> {
    fn new<M: Fn() -> T + ?Sized>(maker: Arc<Box<M>>) -> Self {
        Self {
            last_accessed: Arc::new(Mutex::new(Instant::now())),
            data: Arc::new(Mutex::new(maker())),
        }
    }
}

#[derive(Clone)]
pub struct ResourceManager<T> {
    resources: Arc<Mutex<Vec<Arc<LockedResource<T>>>>>,
    maker: Arc<Box<dyn Fn() -> T + Send + Sync>>,
}


impl <T> ResourceManager<T> where T: Send + Sync + 'static {
    pub fn clone(&self) -> Self {
        Self {
            resources: self.resources.clone(),
            maker: self.maker.clone(),
        }
    }

    pub fn new<M : Fn() -> T + Send + Sync + 'static>(maker: M) -> Self {
        let new = Self {
            resources: Arc::new(Mutex::new(Vec::new())),
            maker: Arc::new(Box::new(maker)),
        };

        // Spawn a background thread for cleanup
        let resource_manager = new.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(60)); // Adjust the interval as needed

                // Remove locks that have been held for too long
                resource_manager.remove_unavailable();
            }
        });

        new
    }

    pub fn get_available_resource(&self) -> Arc<Mutex<T>> {
        self.remove_unavailable();

        let resources = self.resources.lock().expect("Failed to lock resources");
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

        let available = match available {
            Some(resource) => resource,
            None => {
                let resource = Arc::new(LockedResource::new(self.maker.clone()));
                self.add(resource.clone());
                resource
            }
        };

        *available.last_accessed.lock().unwrap() = Instant::now();

        available.data.clone()

    }

    fn remove_unavailable(&self) {
        let mut resources = self.resources.lock().unwrap();
        resources.retain(|resource| {
            let locked = resource.data.try_lock().is_err();
            if resource.last_accessed.lock().unwrap().elapsed() > Duration::from_secs(120)  && locked{
                warn!("Removing unavailable resource");
                false // Don't retain the lock
            } else {
                true // Retain the lock
            }
        });
    }

    fn add(&self, resource: Arc<LockedResource<T>>) {
        let mut resources = self.resources.lock().unwrap();
        resources.push(resource);
    }

}

impl <T: 'static + Send + Sync> serenity::prelude::TypeMapKey for ResourceManager<T> {
    type Value = ResourceManager<T>;
}