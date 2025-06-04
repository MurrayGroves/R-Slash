use std::collections::HashMap;

use log::*;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;

use serenity::all::UserId;
use serenity::futures::TryStreamExt;

use serde_derive::{Deserialize, Serialize};

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

impl<'a> From<&'a mut mongodb::Client> for Client<'a> {
    fn from(client: &'a mut mongodb::Client) -> Client<'a> {
        Client { client }
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
pub async fn get_user_tiers<'a>(
    user: impl Into<String>,
    data: impl Into<mongodb::Client>,
) -> MembershipTiers {
    let client = data.into();

    let user = user.into();

    let db = client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user};
    let mut cursor = coll.find(filter.clone()).await.unwrap();

    let doc = cursor.try_next().await.unwrap().unwrap_or_else(|| {
        // If user information doesn't exist, return a document that indicates no active memberships.
        doc! {
            "active": []
        }
    });

    debug!("User document: {:?}", doc);

    let mut manual = false;
    let bronze_active = match doc.get("tiers") {
        Some(tiers) => {
            let tiers =
                mongodb::bson::from_bson::<HashMap<String, Vec<MembershipDuration>>>(tiers.into())
                    .unwrap_or_else(|_| {
                        let tiers =
                            mongodb::bson::from_bson::<HashMap<String, MembershipDuration>>(
                                tiers.into(),
                            )
                            .unwrap();
                        let mut new_tiers = HashMap::new();
                        for (k, v) in tiers {
                            new_tiers.insert(k, vec![v]);
                        }
                        new_tiers
                    });
            let empty = vec![];
            let bronze = tiers.get("bronze").unwrap_or(&empty);
            let current_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            match bronze.last() {
                Some(tier) => {
                    manual = tier.manual.unwrap_or(false);
                    match tier.end {
                        Some(end) => {
                            if end > current_time && tier.start < current_time {
                                true
                            } else {
                                false
                            }
                        }
                        None => {
                            if tier.start < current_time {
                                true
                            } else {
                                false
                            }
                        }
                    }
                }
                None => false,
            }
        }
        None => false,
    };

    MembershipTiers {
        bronze: MembershipTier {
            _name: "bronze".to_string(),
            active: bronze_active,
            manual,
        },
    }
}
