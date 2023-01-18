use rslash_types::*;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use log::*;

use serenity::futures::TryStreamExt;

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

impl <'a> From<&'a mut tokio::sync::RwLockWriteGuard<'_, serenity::prelude::TypeMap>> for Client<'a> {
    fn from(data: &'a mut tokio::sync::RwLockWriteGuard<'_, serenity::prelude::TypeMap>) -> Client<'a> {
        let mongodb_client: &mut mongodb::Client = match data.get_mut::<ConfigStruct>().unwrap().get_mut("mongodb_connection").unwrap() {
            ConfigValue::MONGODB(db) => Ok(db),
            _ => Err(0),
        }.unwrap();
        Client {
            client: mongodb_client,
        }
    }
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
}

#[derive(Debug)]
pub struct MembershipTiers {
    pub bronze: MembershipTier,
}


// Fetch a user's membership tiers from MongoDB. Returns current status, name of tier, when the user first had the membership, and when the membership expires.
pub async fn get_user_tiers<'a>(user: impl Into<String>, data: impl Into<Client<'a>>) -> MembershipTiers {
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

    let bronze = doc.get_array("active").unwrap().contains(&mongodb::bson::Bson::from("bronze"));

    return MembershipTiers {
        bronze: MembershipTier {
            _name: "bronze".to_string(),
            active: bronze,
        },
    };
}
