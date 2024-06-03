use log::*;
use serde_json::json;
use serenity::all::GuildMemberUpdateEvent;
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
    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Kofi Handler"));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user_id.get().to_string(), "tiers.bronze.end": None::<i64>};
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
    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some(format!("Kofi Handler"));

    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();
    let db = mongodb_client.database("memberships");
    let coll = db.collection::<Document>("users");

    let filter = doc! {"discord_id": user_id.get().to_string()};
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


pub struct ConfigStruct {
    pub mongo_client: mongodb::Client,
    pub posthog: posthog::Client,
}

impl serenity::prelude::TypeMapKey for ConfigStruct {
    type Value = ConfigStruct;
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("{} is connected!", ready.user.name);
        // Store support server info in cache
        ctx.shard.chunk_guild(GuildId::new(697986402117484574), None, false, serenity::gateway::ChunkGuildFilter::None, None);
    }

    async fn guild_member_update(&self, ctx: Context, old: Option<Member>, new: Option<Member>, event: GuildMemberUpdateEvent) {
        debug!("Old: {:?}, New: {:?}", old, new);
        let new = match new {
            Some(new) => new,
            None => return
        };
        let now_donator = new.roles.contains(&RoleId::new(777150304155861013));

        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        let mongodb_client = &mut data_mut.mongo_client;
        let tiers = get_user_tiers(new.user.id.get().to_string(), mongodb_client, None).await;

        debug!("Tiers: {:?}", tiers);
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

            let posthog_client = &mut data_mut.posthog;
            
            posthog_client.capture("donator_change", props, &format!("user_{}", new.user.id.get().to_string())).await.unwrap();
        }
    }


    async fn guild_member_addition(&self, ctx: Context, new: Member) {
        let now_donator = new.roles.contains(&RoleId::new(777150304155861013));

        let mut data = ctx.data.write().await;
        let data_mut = data.get_mut::<ConfigStruct>().unwrap();
        let mongodb_client = &mut data_mut.mongo_client;

        let previously_donator = get_user_tiers(new.user.id.get().to_string(), mongodb_client, None).await.bronze.active;        

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

            let posthog_client = &mut data_mut.posthog;
            
            posthog_client.capture("donator_change", props, &format!("user_{}", new.user.id.get().to_string())).await.unwrap();
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

    let mut client_options = ClientOptions::parse("mongodb://r-slash:r-slash@mongodb-primary.discord-bot-shared.svc.cluster.local/admin?ssl=false").await.unwrap();
    client_options.app_name = Some("Role Detector".to_string());
    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

    let mut client =
        serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");

    {
        let mut data = client.data.write().await;

        let config = ConfigStruct {
            mongo_client: mongodb_client,
            posthog: posthog_client,
        };
        
        data.insert::<ConfigStruct>(config);
    }

    // Finally, start a single shard, and start listening to events.
    //
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        error!("Client error: {:?}", why);
    }
}