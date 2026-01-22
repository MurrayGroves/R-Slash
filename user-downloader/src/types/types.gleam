import gleam/dict
import gleam/dynamic/decode
import gleam/json
import gleam/list
import gleam/option.{type Option}
import gleam/result.{try}
import gleam/string
import processing/embedding
import valkyrie

pub type Post {
  Post(
    /// Reddit ID
    id: String,
    /// No `u/` prefix
    author: String,
    title: String,
    /// URL on Reddit
    url: String,
    embed_urls: List(String),
    /// Unix timestamp
    timestamp: Int,
    score: Int,
    text: Option(String),
    linked_url: Option(String),
    linked_url_image: Option(String),
    linked_url_title: Option(String),
    linked_url_description: Option(String),
  )
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
    media: decode.Dynamic,
    title: String,
  )
}

fn decode_post_response(
  data: decode.Dynamic,
) -> Result(RedditPostResponse, List(decode.DecodeError)) {
  let decoder = {
    use subreddit <- decode.field(
      "subreddit",
      decode.string |> decode.map(string.lowercase),
    )
    use id <- decode.field("id", decode.string)
    use author <- decode.field("author", decode.string)
    use removed_by_category <- decode.field(
      "removed_by_category",
      decode.optional(decode.string),
    )
    use created_utc <- decode.field("created_utc", decode.int)
    use score <- decode.field("score", decode.int)
    use permalink <- decode.field("permalink", decode.string)
    use selftext <- decode.field("selftext", decode.string)
    use is_gallery <- decode.field(
      "is_gallery",
      decode.one_of(decode.bool, [decode.success(False)]),
    )
    use media_mimetypes <- decode.field(
      "media_metadata",
      decode.optional(decode.dict(
        decode.string,
        decode.at(["m"], decode.string),
      )),
    )

    use gallery_data <- decode.field(
      "gallery_data",
      decode.optional(decode.at(
        ["items"],
        decode.list(decode.at(["media_id"], decode.string)),
      )),
    )

    use title <- decode.field("title", decode.string)
    use url <- decode.field("url", decode.string)
    use media <- decode.field("media", decode.dynamic)

    decode.success(RedditPostResponse(
      id:,
      subreddit:,
      author:,
      removed_by_category:,
      created_utc:,
      score:,
      permalink:,
      selftext:,
      is_gallery:,
      gallery_data:,
      media_mimetypes:,
      url:,
      media:,
      title:,
    ))
  }

  decode.run(data, decoder)
}

pub type PostDecodingError {
  AccessingDataChildren(json.DecodeError)
  DecodingChildren(List(decode.DecodeError))
}

pub fn decode_posts_response(
  json: String,
) -> Result(List(RedditPostResponse), PostDecodingError) {
  let decoder = decode.at(["data", "children"], decode.list(decode.dynamic))

  use dynamic <- try(
    json.parse(from: json, using: decoder)
    |> result.map_error(AccessingDataChildren),
  )

  dynamic
  |> list.map(decode_post_response)
  |> result.all
  |> result.map_error(DecodingChildren)
}

pub type ContentType {
  TextOnly
  MediaOnly
  Both
}

pub fn content_allowed(allow_level: ContentType, post_type: ContentType) -> Bool {
  case allow_level {
    Both -> True
    _ -> allow_level == post_type
  }
}

pub type PostProcessingError {
  RedisError(valkyrie.Error)
  FailedDeletion
  MissingExpectedGallery
  DecodeError(List(decode.DecodeError))
  InvalidMIMEType
  /// Shouldn't happen unless Reddit messes up
  MediaItemMissingMimeType
  OpenGraphAnalysisError(embedding.OpenGraphAnalysisError)
}

pub type LinkEmbed {
  LinkEmbed(
    title: Option(String),
    description: Option(String),
    image: Option(String),
  )
}

pub type Post {
  Post(
    id: String,
    author: String,
    title: String,
    reddit_url: String,
    embed_urls: List(String),
    timestamp: Int,
    score: Int,
  )
}
