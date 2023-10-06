use std::collections::HashMap;

use anyhow::Error;

pub struct Client<'a> {
    path: &'a str,
}

impl <'a>Client<'a> {
    pub fn new(path: &'a str) -> Self {
        Self {
            path,
        }
    }

    pub async fn request(&self, url: &str) -> Result<String, Error> {
        todo!();
    }

    pub async fn request_batch(&self, urls: Vec<&str>) -> Result<HashMap<String, String>, Error> {
        todo!();
    }
}