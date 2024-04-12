use std::collections::HashMap;
use std::sync::Arc;

use rslash_types::*;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use log::*;

use serenity::futures::TryStreamExt;

use serde_derive::{Deserialize, Serialize};
use serenity::prelude::TypeMap;
use tokio::sync::RwLock;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}


pub struct Client<'a> {
    client: &'a mut mongodb::Client,
}


impl <'a> From<&'a mut mongodb::Client> for Client<'a> {
    fn from(client: &'a mut mongodb::Client) -> Client<'a> {
        Client  {
            client,
        }
    }
}


#[derive(Debug)]
pub struct MembershipTier {
    pub _name: String,
    pub active: bool,
    pub manual: bool,
}

#[derive(Debug)]
pub struct MembershipTiers {
    pub bronze: MembershipTier,
}


#[derive(Deserialize, Serialize)]
struct MembershipDuration {
    start: u64,
    end: Option<u64>,
    manual: Option<bool>,
}


// Fetch a user's membership tiers from MongoDB. Returns current status, name of tier, when the user first had the membership, and when the membership expires.
pub async fn get_user_tiers<'a>(user: impl Into<String>, data: impl Into<Client<'a>>, parent_tx: Option<&sentry::TransactionOrSpan>) -> MembershipTiers {
    let span: sentry::TransactionOrSpan = match &parent_tx {
        Some(parent) => parent.start_child("db.query", "get_user_tiers").into(),
        None => {
            let ctx = sentry::TransactionContext::new("db.query", "get_user_tiers");
            sentry::start_transaction(ctx).into()
        }
    };

    let client: Client = data.into();
    let mongodb_client = client.client;

    let user = user.into();

    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user};
    let find_options = FindOptions::builder().build();
    let mut cursor = coll.find(filter.clone(), find_options.clone()).await.unwrap();

    let doc = match cursor.try_next().await.unwrap() {
        Some(doc) => doc, // If user information exists, return it
        None => { // If user information doesn't exist, return a document that indicates no active memberships.
            doc! {
                "active": []
            }
        }
    };

    debug!("User document: {:?}", doc);

    let mut manual = false;
    let bronze_active = match doc.get("tiers") {
        Some(tiers) => {
            let tiers = mongodb::bson::from_bson::<HashMap<String, MembershipDuration>>(tiers.into()).unwrap();
            let bronze = tiers.get("bronze");
            let current_time = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

            match bronze {
                Some(tier) => {
                    manual = tier.manual.unwrap_or(false);
                    match tier.end {
                        Some(end) => {
                            if end > current_time && tier.start < current_time {
                                true
                            } else {
                                false
                            }
                        },
                        None => {
                            if tier.start < current_time {
                                true
                            } else {
                                false
                            }
                        }
                    }
                },
                None => false,
            }
        },
        None => false,
    };
    

    span.finish();
    return MembershipTiers {
        bronze: MembershipTier {
            _name: "bronze".to_string(),
            active: bronze_active,
            manual
        },
    };
}
