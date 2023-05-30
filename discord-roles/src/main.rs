use log::*;
use serde_json::json;
use std::collections::HashMap;
use std::io::Write;
use std::env;

use serenity::async_trait;
use serenity::model::gateway::Ready;
use serenity::prelude::*;
use serenity::model::guild::*;
use serenity::model::id::*;
use serenity::model::Timestamp;

use mongodb::{options::ClientOptions};
use mongodb::bson::{doc, Document};

use memberships::get_user_tiers;
use rslash_types::*;


// Terminate a user's current membership period
async fn terminate_membership(user_id: UserId) {
    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Kofi Handler"));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user_id.0.to_string(), "tiers.bronze.end": None::<i64>};
    let user = doc! {
        "$set" : {
            "tiers.bronze.$.end": Timestamp::now().unix_timestamp()
        },
        "$pull" : {
            "active": {"$in" : ["bronze"]} // Remove bronze from active
        }
    };

    let options = mongodb::options::UpdateOptions::builder().upsert(true).build();
    coll.update_one(filter, user, options).await.unwrap();}


// Start a new membership period for the user
async fn add_membership(user_id: UserId) {
    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Kofi Handler"));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user_id.0.to_string()};
    let user = doc! {
            "$push" : {
                "tiers.bronze": {
                    "start": Timestamp::now().unix_timestamp(),
                    "end": None::<i64>
                }
            },
            "$set": {
                "active": ["bronze"]
            }
    };

    let options = mongodb::options::UpdateOptions::builder().upsert(true).build();
    coll.update_one(filter, user, options).await.unwrap();
}

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);
        // Store support server info in cache
        ctx.shard.chunk_guild(GuildId(697986402117484574), None, serenity::client::bridge::gateway::ChunkGuildFilter::None, None);
    }

    async fn guild_member_update(&self, ctx: Context, old: Option<Member>, new: Member) {
        debug!("Old: {:?}, New: {:?}", old, new);
        let now_donator = new.roles.contains(&RoleId(777150304155861013));

        let tiers = get_user_tiers(new.user.id.0.to_string(), &mut ctx.data.write().await, None).await;

        if tiers.bronze.manual { // If the user has a manual bronze membership, don't do anything
            return;
        }

        let previously_donator = tiers.bronze.active;        

        debug!("Previously donator: {}, now donator: {}", previously_donator, now_donator);

        let mut donator: Option<bool> = None;
        if now_donator && !previously_donator {
            info!("{} ({}) is now a donator", new.user.name, new.user.id);
            add_membership(new.user.id).await;
            donator = Some(true);
        } else if !now_donator && previously_donator {
            info!("{} ({}) is no longer a donator", new.user.name, new.user.id);
            terminate_membership(new.user.id).await;
            donator = Some(false);

        }

        if donator.is_some() {
            let props = json!({
                "$set": {
                    "donator": donator
                }
            });

            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();
            let posthog_client: &mut posthog::Client = match data_mut.get_mut("posthog").unwrap() {
                ConfigValue::PosthogClient(client) => client,
                _ => panic!("Expected posthog client")
            };
            
            posthog_client.capture("donator_change", props, &format!("user_{}", new.user.id.0.to_string())).await.unwrap();
        }
    }


    async fn guild_member_addition(&self, ctx: Context, new: Member) {
        let now_donator = new.roles.contains(&RoleId(777150304155861013));

        let previously_donator = get_user_tiers(new.user.id.0.to_string(), &mut ctx.data.write().await, None).await.bronze.active;        

        debug!("Previously donator: {}, now donator: {}", previously_donator, now_donator);

        let mut donator: Option<bool> = None;
        if now_donator && !previously_donator {
            info!("{} ({}) is now a donator", new.user.name, new.user.id);
            add_membership(new.user.id).await;
            donator = Some(true);
        } else if !now_donator && previously_donator {
            info!("{} ({}) is no longer a donator", new.user.name, new.user.id);
            terminate_membership(new.user.id).await;
            donator = Some(false);

        }

        if donator.is_some() {
            let props = json!({
                "$set": {
                    "donator": donator
                }
            });

            let mut data = ctx.data.write().await;
            let data_mut = data.get_mut::<ConfigStruct>().unwrap();
            let posthog_client: &mut posthog::Client = match data_mut.get_mut("posthog").unwrap() {
                ConfigValue::PosthogClient(client) => client,
                _ => panic!("Expected posthog client")
            };
            
            posthog_client.capture("donator_change", props, &format!("user_{}", new.user.id.0.to_string())).await.unwrap();
        }
    }
}


#[tokio::main]
async fn main() {
    env_logger::builder()
    .format(|buf, record| {
        writeln!(buf, "{}: {}", record.level(), record.args())
    })
    .init();

    debug!("Starting...");

    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");
    // Set gateway intents, which decides what events the bot will be notified about
    let intents = GatewayIntents::GUILD_MEMBERS | GatewayIntents::non_privileged();

    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some("Role Detector".to_string());
    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

    let mut client =
        serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");

    {
        let mut data = client.data.write().await;
        let mut hashmap = HashMap::new();
        hashmap.insert("posthog".to_string(), ConfigValue::PosthogClient(posthog_client));
        hashmap.insert("mongodb_connection".to_string(), ConfigValue::MONGODB(mongodb_client));
        
        data.insert::<ConfigStruct>(hashmap);
    }

    // Finally, start a single shard, and start listening to events.
    //
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}