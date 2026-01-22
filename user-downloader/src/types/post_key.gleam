import gleam/string

pub type PostKey {
  PostKeyString(key: String)
  PostKey(subreddit: String, id: String)
}

pub fn to_string(key: PostKey) -> String {
  case key {
    PostKeyString(x) -> {
      x
    }
    PostKey(subreddit, id) -> {
      "subreddit:" <> subreddit <> ":post:" <> id
    }
  }
}

pub fn get_subreddit(key: PostKey) -> String {
  case key {
    PostKeyString(x) -> {
      case string.split(x, ":") {
        [_, subreddit, ..] -> subreddit
        _ -> panic
      }
    }
    PostKey(subreddit, _) -> subreddit
  }
}
