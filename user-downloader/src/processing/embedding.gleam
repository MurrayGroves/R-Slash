import gleam/dict
import gleam/dynamic/decode
import gleam/list
import gleam/option.{None, Some}
import gleam/result.{try}
import gleam/string
import marceau

import types/types.{
  type LinkEmbed, type PostProcessingError, DecodeError, InvalidMIMEType,
  MediaItemMissingMimeType, MissingExpectedGallery,
}

pub type PostMedia {
  RedditGallery(media: List(String))
  SingleEmbeddable(String)
  SingleNeedsConverting(String)
  NotEmbeddable
  NoMedia
}

/// Extract URLs for a post that we know has a gallery attached
fn extract_gallery_urls(
  post: types.RedditPostResponse,
) -> Result(List(String), PostProcessingError) {
  case post.media_mimetypes, post.gallery_data {
    Some(media_mimetypes), Some(gallery_data) -> {
      let urls =
        dict.map_values(media_mimetypes, fn(id, mime_type) {
          let mime_type = case mime_type {
            "image/jpg" -> "image/jpeg"
            x -> x
          }
          case list.first(marceau.mime_type_to_extensions(mime_type)) {
            Ok(ext) -> Ok("https://i.redd.it" <> id <> "." <> ext)
            Error(_) -> Error(InvalidMIMEType)
          }
        })

      list.map(gallery_data, fn(id) {
        dict.get(urls, id) |> result.replace_error(MediaItemMissingMimeType)
      })
      |> result.all
      |> result.map(result.all)
      |> result.flatten
    }
    _, _ -> Error(MissingExpectedGallery)
  }
}

/// Gets the media attached to a post which doesn't have a Reddit gallery attached
fn get_post_single_media(
  post: types.RedditPostResponse,
) -> Result(PostMedia, PostProcessingError) {
  let url = post.url
  case string.starts_with(url, "https://v.redd.it/") {
    True -> {
      decode.run(
        post.media,
        decode.at(["reddit_video", "dash_url"], decode.string),
      )
      |> result.map(fn(dash_url) { SingleNeedsConverting(dash_url) })
      |> result.map_error(DecodeError)
    }
    False ->
      Ok({
        case
          list.any([".gif", ".png", ".jpg", ".jpeg", ".mp4"], fn(ext) {
            string.ends_with(url, ext)
          })
          && !string.contains(url, "redgifs.com")
        {
          True -> SingleEmbeddable(url)
          False ->
            case
              list.any(["imgur.com", "redgifs.com", ".mpd"], fn(convertable) {
                string.contains(url, convertable)
              })
            {
              True -> SingleNeedsConverting(url)
              False ->
                case string.starts_with(url, "https://reddit.com/") {
                  True -> NoMedia
                  False -> NotEmbeddable
                }
            }
        }
      })
  }
}

pub fn convert_post_media(
  post: types.RedditPostResponse,
  url: String,
) -> Result(PostMedia, PostProcessingError) {
  todo
}

pub fn get_post_media(
  post: types.RedditPostResponse,
) -> Result(PostMedia, PostProcessingError) {
  case post.is_gallery {
    True -> {
      extract_gallery_urls(post) |> result.map(fn(x) { RedditGallery(x) })
    }
    False -> get_post_single_media(post)
  }
}

pub type OpenGraphAnalysisError {
  RequestError
  ParseError
}

pub fn get_link_embed(
  post: types.RedditPostResponse,
) -> Result(LinkEmbed, OpenGraphAnalysisError) {
  todo
}
