import given
import gleam/dict
import gleam/dynamic/decode
import gleam/int
import gleam/list
import gleam/option.{type Option, None, Some}
import gleam/result.{try}
import logging
import processing/embedding
import valkyrie
import valkyrie/pipeline

import gleam/erlang/process.{type Subject}
import gleam/otp/actor

import types/post_key.{type PostKey}
import types/types.{
  type PostProcessingError, DecodeError, FailedDeletion, InvalidMIMEType,
  MediaItemMissingMimeType, MissingExpectedGallery, RedisError,
}

fn delete_post(
  post_list_manager: Subject(AddPostMessage),
  redis: valkyrie.Connection,
  post_key: PostKey,
) {
  process.send(post_list_manager, DeletePost(post_key))
  case valkyrie.del(redis, [post_key.to_string(post_key)], 1000) {
    Ok(deleted_keys) if deleted_keys != 1 -> {
      Error(FailedDeletion)
    }
    Ok(_) -> Ok(None)
    Error(e) -> Error(RedisError(e))
  }
}

/// Update the score of an existing post
pub fn process_existing_post(
  redis: valkyrie.Connection,
  post_key: PostKey,
  post: types.RedditPostResponse,
) -> Result(Option(PostKey), PostProcessingError) {
  case
    valkyrie.hset(
      redis,
      post_key.to_string(post_key),
      dict.from_list([#("score", int.to_string(post.score))]),
      1000,
    )
  {
    Ok(_) -> Ok(Some(post_key))
    Error(e) -> {
      logging.log(
        logging.Warning,
        "Failed to set score for post " <> post_key.to_string(post_key),
      )
      Error(RedisError(e))
    }
  }
}

/// Process a post that hasn't seen before and add it to Redis
pub fn process_new_post(
  redis: valkyrie.Connection,
  post_list_manager: Subject(AddPostMessage),
  post_key: PostKey,
  post: types.RedditPostResponse,
) -> Result(Option(PostKey), PostProcessingError) {
  use media <- try(embedding.get_post_media(post))

  use link_embed <- try(embedding.get_link_embed(post))

  process.send(
    post_list_manager,
    MarkDone(PostState(True, types.Both, post_key)),
  )

  Ok(Some(post_key))
}

/// Process a post and add to Redis, returning key of post is successful
pub fn process_post(
  redis: valkyrie.Connection,
  existing_posts: List(String),
  post_list_manager: Subject(AddPostMessage),
  post: types.RedditPostResponse,
) -> Result(Option(PostKey), PostProcessingError) {
  let key = post_key.PostKey(subreddit: post.subreddit, id: post.id)

  let post_already_exists =
    list.contains(existing_posts, post_key.to_string(key))

  let post_deleted_on_reddit =
    option.is_some(post.removed_by_category) || post.author == "[deleted]"

  case post_deleted_on_reddit {
    True -> {
      case post_already_exists {
        True -> delete_post(post_list_manager, redis, key)
        False -> Ok(None)
      }
    }
    False -> {
      case post_already_exists {
        True -> process_existing_post(redis, key, post)
        False -> process_new_post(redis, post_list_manager, key, post)
      }
    }
  }
}

pub type PostState {
  PostState(added_to_redis: Bool, content_type: types.ContentType, key: PostKey)
}

type AddPostActorState {
  AddPostActorState(
    redis: valkyrie.Connection,
    posts: List(PostState),
    subreddit: String,
  )
}

pub type AddPostMessage {
  Shutdown
  MarkDone(post: PostState)
  DeletePost(key: PostKey)
}

pub fn start_post_list_manager(
  redis: valkyrie.Connection,
  posts: List(types.RedditPostResponse),
  subreddit: String,
) -> Subject(AddPostMessage) {
  let assert Ok(actor) =
    actor.new(AddPostActorState(
      redis:,
      posts: posts
        |> list.map(fn(post) {
          PostState(
            False,
            types.MediaOnly,
            post_key.PostKey(subreddit:, id: post.id),
          )
        }),
      subreddit:,
    ))
    |> actor.on_message(handle_message)
    |> actor.start

  actor.data
}

fn update_post_lists(
  redis redis: valkyrie.Connection,
  new_post_type new_post_type: types.ContentType,
  subreddit subreddit: String,
  all_posts all_posts: List(PostState),
) -> Result(Nil, valkyrie.Error) {
  // Must update Both list
  let both_list = all_posts |> list.map(fn(x) { post_key.to_string(x.key) })

  let subreddit_key = "subreddit:" <> subreddit <> ":posts"

  use _ <- try(
    pipeline.new()
    |> pipeline.del([subreddit_key])
    |> pipeline.rpush(subreddit_key, both_list)
    |> pipeline.exec_transaction(redis, 5000),
  )

  // May have to update either TextOnly or MediaOnly list
  use _ <- try(case new_post_type {
    types.TextOnly -> {
      let text_only =
        all_posts
        |> list.filter_map(fn(x) {
          case x.content_type {
            types.TextOnly -> Ok(post_key.to_string(x.key))
            _ -> Error(Nil)
          }
        })

      let subreddit_key = "subreddit:" <> subreddit <> ":posts:text"

      use _ <- try(
        pipeline.new()
        |> pipeline.del([subreddit_key])
        |> pipeline.rpush(subreddit_key, text_only)
        |> pipeline.exec_transaction(redis, 5000),
      )

      Ok(Nil)
    }
    types.MediaOnly -> {
      let media_only =
        all_posts
        |> list.filter_map(fn(x) {
          case x.content_type {
            types.MediaOnly -> Ok(post_key.to_string(x.key))
            _ -> Error(Nil)
          }
        })

      let subreddit_key = "subreddit:" <> subreddit <> ":posts:media"

      use _ <- try(
        pipeline.new()
        |> pipeline.del([subreddit_key])
        |> pipeline.rpush(subreddit_key, media_only)
        |> pipeline.exec_transaction(redis, 5000),
      )

      Ok(Nil)
    }
    _ -> Ok(Nil)
  })

  Ok(Nil)
}

fn handle_message(
  state: AddPostActorState,
  message: AddPostMessage,
) -> actor.Next(AddPostActorState, AddPostMessage) {
  case message {
    Shutdown -> actor.stop()

    MarkDone(new_post) -> {
      // Update post to be done
      let all_posts =
        state.posts
        |> list.map(fn(post) {
          case post.key == new_post.key {
            True -> new_post
            False -> post
          }
        })
        |> list.filter(fn(post) { post.added_to_redis })

      let res =
        update_post_lists(
          redis: state.redis,
          new_post_type: new_post.content_type,
          subreddit: state.subreddit,
          all_posts:,
        )

      case res {
        Ok(_) -> Nil
        Error(e) -> {
          logging.log(
            logging.Warning,
            "Failed to update subreddit lists for "
              <> state.subreddit
              <> " because of Redis error",
          )
        }
      }

      actor.continue(AddPostActorState(state.redis, all_posts, state.subreddit))
    }

    DeletePost(key) -> {
      let post_key = post_key.to_string(key)
      let subreddit = post_key.get_subreddit(key)
      let res =
        fn() {
          use _ <- try(
            pipeline.new()
            |> pipeline.lrem("subreddit:" <> subreddit <> ":posts", 1, post_key)
            |> pipeline.lrem(
              "subreddit:" <> subreddit <> ":posts:text",
              1,
              post_key,
            )
            |> pipeline.lrem(
              "subreddit:" <> subreddit <> ":posts:media",
              1,
              post_key,
            )
            |> pipeline.exec(state.redis, 5000),
          )
          Ok(Nil)
        }()

      case res {
        Ok(_) -> {
          Nil
        }
        Error(e) -> {
          logging.log(
            logging.Warning,
            "Failed to update subreddit lists after deleting "
              <> post_key
              <> " because of Redis error",
          )
        }
      }

      let posts = state.posts |> list.filter(fn(x) { x.key != key })
      actor.continue(AddPostActorState(state.redis, posts, state.subreddit))
    }
  }
}
