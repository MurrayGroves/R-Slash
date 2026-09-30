import gleam/dynamic/decode
import gleam/option.{type Option}
import gleam/dict

///A post can either be in a subreddit, or a user's profile
pub type PostLocation {
  Subreddit(String)
  User(String)
}

pub type RedditPostResponse {
  RedditPostResponse(
    subreddit: String,
    id: String,
    author: String,
    removed_by_category: Option(String),
    created_utc: Int,
    score: Int,
    permalink: String,
    selftext: String,
    is_gallery: Bool,
    gallery_data: Option(List(String)),
    media_mimetypes: Option(dict.Dict(String, String)),
    url: String,
    /// Dynamic because has different shape depending on type of media (gallery, video, img, etc)
    media: decode.Dynamic,
    title: String,
  )
}

fn reddit_post_response_decoder() -> decode.Decoder(RedditPostResponse) {
  use subreddit <- decode.field("subreddit", decode.string)
  use id <- decode.field("id", decode.string)
  use author <- decode.field("author", decode.string)
  use removed_by_category <- decode.field("removed_by_category", decode.optional(decode.string))
  use created_utc <- decode.field("created_utc", decode.int)
  use score <- decode.field("score", decode.int)
  use permalink <- decode.field("permalink", decode.string)
  use selftext <- decode.field("selftext", decode.string)
  use is_gallery <- decode.field("is_gallery", decode.bool)
  use gallery_data <- decode.field("gallery_data", decode.optional(decode.list(decode.string)))
  use media_mimetypes <- decode.field("media_mimetypes", decode.optional(decode.dict(decode.string, decode.string)))
  use url <- decode.field("url", decode.string)
  use media <- decode.field("media", decode.dynamic)
  use title <- decode.field("title", decode.string)
  decode.success(RedditPostResponse(subreddit:, id:, author:, removed_by_category:, created_utc:, score:, permalink:, selftext:, is_gallery:, gallery_data:, media_mimetypes:, url:, media:, title:))
}
