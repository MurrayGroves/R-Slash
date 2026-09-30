import gleam/erlang/process
import mungo
import valkyrie
import carotte
import types/reddit


fn determine_next_location(redis: valkyrie.Connection, mongodb: process.Subject(mungo.Message)) -> reddit.PostLocation {
  let sources: List(reddit.PostLocation) = []

  // Built-in subreddits
  let config = mongodb |> mungo.collection("config.settings")

  let subreddit_list: List(String) = config |> mungo.find_by_id("subreddit_list", 30)

  let subreddit_fetch_times = map(subreddit_list, valkyrie.get(redis, ))

  todo
}

fn fetch_location()

/// Loops over the queue of reddit locations and fetches the post metadata from them.
fn reddit_loop(mq: carotte.Channel, redis: valkyrie.Connection, mongodb: process.Subject(mungo.Message)) {
  let location = determine_next_location(redis, mongodb)

  reddit_loop(mq, redis, mongodb)
}

pub fn main() {
  // Connect to RabbitMQ
  let assert Ok(client) =
    carotte.ClientConfig(
      ..carotte.default_client(),
      host: "localhost",
      port: 5672,
    )
    |> carotte.start()

  let assert Ok(mq_channel) = carotte.open_channel(client)

  // Create exchange for processable posts
  let assert Ok(_) =
    carotte.Exchange(
      ..carotte.exchange("processable_posts"),
      exchange_type: carotte.Direct,
    )
    |> carotte.declare_exchange(mq_channel)
}
