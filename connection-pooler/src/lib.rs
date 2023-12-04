use std::sync::{Arc, Mutex};
use std::cell::RefCell;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct LockedResource<T> {
    lock: Arc<Mutex<()>>,
    last_accessed: Arc<Mutex<Instant>>,
    data: Arc<Mutex<T>>,
}

impl <T> LockedResource<T> {
    fn new<M: Fn() -> T + ?Sized>(maker: Arc<Box<M>>) -> Self {
        Self {
            lock: Arc::new(Mutex::new(())),
            last_accessed: Arc::new(Mutex::new(Instant::now())),
            data: Arc::new(Mutex::new(maker())),
        }
    }
}

struct ResourceManager<T> {
    resources: Arc<Mutex<Vec<Arc<LockedResource<T>>>>>,
    maker: Arc<Box<dyn Fn() -> T + Send + Sync>>,
}

impl <T> ResourceManager<T> where T: Send + Sync + 'static {
    fn new<M : Fn() -> T + Send + Sync + 'static>(maker: M) -> Arc<Self> {
        let new = Self {
            resources: Arc::new(Mutex::new(Vec::new())),
            maker: Arc::new(Box::new(maker)),
        };

        // Spawn a background thread for cleanup
        let resource_manager = Arc::new(new);
        let to_return = resource_manager.clone();
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(5)); // Adjust the interval as needed

                // Remove locks that have been held for too long
                resource_manager.remove_unavailable();
                let lock = resource_manager.resources.lock().expect("Failed to lock");
                println!("Removed deadlocks, new resources length: {}", lock.len());
            }
        });

        to_return
    }

    fn get_available_resource(&self) -> Arc<LockedResource<T>> {
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

        available

    }

    fn remove_unavailable(&self) {
        let mut resources = self.resources.lock().unwrap();
        resources.retain(|resource| {
            if resource.last_accessed.lock().unwrap().elapsed() > Duration::from_secs(10) {
                println!("Removing lock");
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
