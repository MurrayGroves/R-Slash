use serenity::{all::EventHandler, async_trait};

struct Handler;

#[async_trait]
impl EventHandler for Handler {}

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
    let intents = GatewayIntents::empty();
    let posthog_key: String = env::var("POSTHOG_API_KEY").expect("POSTHOG_API_KEY not set").parse().expect("Failed to convert POSTHOG_API_KEY to string");
    let posthog_client = posthog::Client::new(posthog_key, "https://eu.posthog.com/capture".to_string());

    let mut client_options = ClientOptions::parse("mongodb+srv://my-user:rslash@mongodb-svc.r-slash.svc.cluster.local/admin?replicaSet=mongodb&ssl=false").await.unwrap();
    client_options.app_name = Some("Post Subscriber".to_string());
    let mongodb_client = mongodb::Client::with_options(client_options).unwrap();

    let mut client =
        serenity::Client::builder(&token, intents).event_handler(Handler).await.expect("Err creating client");
}