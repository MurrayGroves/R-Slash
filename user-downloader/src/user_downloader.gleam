import crew
import gleam/dict
import gleam/dynamic/decode
import gleam/erlang/process
import gleam/http/request
import gleam/httpc
import gleam/int
import gleam/io
import gleam/json
import gleam/list
import gleam/option.{type Option, None, Some}
import gleam/otp/actor
import gleam/otp/static_supervisor as supervisor
import gleam/result.{map_error, try}
import gleam/string
import logging
import mug
import processing/processing
import valkyrie/resp

import envoy
import valkyrie

import types/types

/// Download a given subreddit/user. Users should be prefixed with `u_`
fn download_subreddit(
  redis: valkyrie.Connection,
  subreddit: String,
) -> Result(Nil, DownloadError) {
  use subreddit_name <- try(case string.starts_with(subreddit, "u/") {
    True -> {
      Error(StringError("User prefixed with u/"))
    }
    False -> {
      Ok(subreddit)
    }
  })

  let url = "http://reddit-proxy/subreddit/" <> subreddit_name

  let assert Ok(req) = request.to(url)

  use resp <- try(httpc.send(req) |> result.map_error(HttpError))

  use body <- try(case resp.status {
    200 -> {
      Ok(resp.body)
    }
    _ -> {
      Error(StringError(
        "Response from Reddit proxy was "
        |> string.append(int.to_string(resp.status))
        |> string.append(", expected 200"),
      ))
    }
  })

  use posts <- try(
    types.decode_posts_response(body) |> result.map_error(PostDecodeError),
  )

  let post_list_manager =
    processing.start_post_list_manager(redis, posts, subreddit)

  let pool_name = process.new_name("post_processing_pool")
  use _ <- try(
    crew.new(pool_name, fn(x) {
      processing.process_post(redis, [], post_list_manager, x)
    })
    |> crew.fixed_size(4)
    |> crew.start
    |> map_error(PoolStartError),
  )

  // Timeout of 10 mins to finish all posts
  posts
  |> crew.call_parallel(pool_name, 10 * 60 * 1000, _)
  |> list.filter_map(fn(res) {
    case res {
      Ok(_) -> res
      Error(e) -> {
        logging.log(logging.Warning, "A post failed to be processed")
        res
      }
    }
  })

  Ok(Nil)
}

fn download_next_user_subscription(redis: valkyrie.Connection) {
  todo
}

type DownloadError {
  RedisError(error: valkyrie.Error)
  StringError(error: String)
  NothingQueuedError
  HttpError(httpc.HttpError)
  PostDecodeError(types.PostDecodingError)
  PoolStartError(actor.StartError)
}

fn download_next_user_immediate(
  redis: valkyrie.Connection,
) -> Result(Nil, DownloadError) {
  use user_requests <- try(
    valkyrie.hgetall(redis, "user_fetch_requests", 1000)
    |> result.map_error(RedisError),
  )

  use oldest <- try(
    dict.fold(user_requests, Ok(#(None, 0)), fn(acc, key, value) {
      use value <- try(case value {
        resp.Integer(x) -> Ok(x)
        resp.BigNumber(x) -> Ok(x)
        _ -> Error(StringError("Request timestamp not integer"))
      })

      use acc <- try(acc)

      case acc.0 {
        None -> Ok(#(Some(key), value))
        Some(_) -> {
          case value < acc.1 {
            True -> Ok(#(Some(key), value))
            False -> Ok(acc)
          }
        }
      }
    }),
  )

  use oldest <- try(oldest.0 |> option.to_result(NothingQueuedError))

  use user <- try(case oldest {
    resp.BulkString(x) -> Ok(x)
    resp.SimpleString(x) -> Ok(x)
    _ -> Error(StringError("User key not string"))
  })

  use _ <- try(download_subreddit(redis, user))

  download_next_user_immediate(redis)
}

pub fn main() -> Result(Nil, Nil) {
  io.println("user_downloader starting")
  logging.configure()
  logging.log(logging.Info, "Logging initialised")

  let redis_pool_name = process.new_name("redis_connection_pool")

  use redis_addr <- try(envoy.get("REDIS_ADDR"))

  // Define a pool of 10 connections
  let valkyrie_child_spec =
    valkyrie.Config(redis_addr, 6379, valkyrie.NoAuth, mug.Ipv6Preferred)
    |> valkyrie.supervised_pool(
      size: 10,
      name: option.Some(redis_pool_name),
      timeout: 1000,
    )

  // Start the pool under a supervisor
  let assert Ok(_started) =
    supervisor.new(supervisor.OneForOne)
    |> supervisor.add(valkyrie_child_spec)
    |> supervisor.start

  // Get the connection now that the pool is started
  let redis = valkyrie.named_connection(redis_pool_name)

  download_next_user_immediate(redis)

  Ok(Nil)
}
