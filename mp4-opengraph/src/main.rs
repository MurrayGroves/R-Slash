use std::net::SocketAddr;
use std::path;
use std::sync::Arc;

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use anyhow::{anyhow, Context, Error};

#[derive(serde::Deserialize)]
struct PostQuery {
    title: String,
}

async fn handle(req: Request<hyper::body::Incoming>, base_html: Arc<String>) -> Result<Response<Full<Bytes>>, Error> {
    let uri = req.uri().to_string();
    if !uri.starts_with("/render") {
        return Ok(Response::new(Full::new(Bytes::from("Invalid URL"))));
    }

    let url = url::Url::parse(
        &format!("https://r-slash.b-cdn.net{}", uri)
    ).context("Converting into URL")?;
    let params: PostQuery = serde_qs::from_str(url.query().unwrap_or("Invalid params"))?;

    let video = url.path().split("/render").collect::<Vec<&str>>().get(1)
        .ok_or(anyhow!("Couldn't parse url"))?.split("?").next()
        .ok_or(anyhow!("Couldn't remove query from url"))?;

    let video = format!("https://r-slash.b-cdn.net/gifs{}", video);

    let mut html: String = base_html.to_string();
    html = html.replace("REPLACE_TITLE", &params.title);
    html = html.replace("REPLACE_VIDEO", &video);

    Ok(Response::new(Full::new(Bytes::from(html))))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let base = Arc::new(std::fs::read_to_string(path::Path::new("base.html"))?);

    let addr = SocketAddr::from(([0, 0, 0, 0], 8081));

    // We create a TcpListener and bind it to 127.0.0.1:8081
    let listener = TcpListener::bind(addr).await?;

    // We start a loop to continuously accept incoming connections
    loop {
        let (stream, _) = listener.accept().await?;

        // Use an adapter to access something implementing `tokio::io` traits as if they implement
        // `hyper::rt` IO traits.
        let io = TokioIo::new(stream);
        let base = Arc::clone(&base);

        // Spawn a tokio task to serve multiple connections concurrently
        tokio::task::spawn(async move {
            // Finally, we bind the incoming connection to our `hello` service
            if let Err(err) = http1::Builder::new()
                // `service_fn` converts our function in a `Service`
                .serve_connection(io, service_fn(|req| handle(req, base.clone())))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}