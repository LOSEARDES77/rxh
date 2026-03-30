//! Proxy specific sub-service. See also [`crate::http`] module.

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::OnceLock,
    time::{Duration, Instant},
};

use bytes::Bytes;
use http_body_util::{BodyExt, combinators::BoxBody};
use hyper::{Response, body::Incoming, header, upgrade::OnUpgrade};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::{net::TcpStream, sync::RwLock};

use crate::{
    config,
    http::{
        request::ProxyRequest,
        response::{BoxBodyResponse, LocalResponse, ProxyResponse},
    },
};

static CACHE: OnceLock<RwLock<HashMap<String, CacheEntry>>> = OnceLock::new();

fn cache_store() -> &'static RwLock<HashMap<String, CacheEntry>> {
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

#[derive(Clone)]
struct CacheEntry {
    status: http::StatusCode,
    version: http::Version,
    headers: hyper::HeaderMap,
    body: Bytes,
    expires_at: Instant,
}

/// Forwards the request to the target server and returns the response sent
/// by the target server. See [`ProxyRequest`] and [`ProxyResponse`]. If the
/// client wants to upgrade the connection and the server agrees by sending
/// a `101` status code, then a TCP tunnel that forwards traffic bidirectionally
/// is spawned in a new Tokio task. See [`tunnel`].
pub(super) async fn forward(
    mut request: ProxyRequest<Incoming>,
    to: SocketAddr,
    response_headers: config::ResponseHeaders,
    received_by: String,
    cache: config::Cache,
) -> Result<BoxBodyResponse, hyper::Error> {
    let method = request.method().clone();
    let uri = request.uri().to_string();
    let should_cache = cache.enabled
        && method == http::Method::GET
        && !request.headers().contains_key(header::UPGRADE);
    let cache_key = format!("{to}|{method}|{uri}");

    if should_cache && let Some(cached) = get_cached_response(&cache_key).await {
        return Ok(cached);
    }

    let Ok(stream) = TcpStream::connect(to).await else {
        return Ok(LocalResponse::bad_gateway());
    };

    let use_http2 = request.version() == http::Version::HTTP_2;
    let mut maybe_client_upgrade = None;

    if !use_http2 && request.headers().contains_key(header::UPGRADE) {
        maybe_client_upgrade = Some(request.on_upgrade());
    }

    let forwarded = if maybe_client_upgrade.is_some() {
        request
            .into_forwarded()
            .map(|_| crate::http::body::empty() as BoxBody<Bytes, hyper::Error>)
    } else {
        request
            .into_forwarded()
            .map(|body| body.boxed() as BoxBody<Bytes, hyper::Error>)
    };

    let mut response = if use_http2 {
        let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
            .handshake(TokioIo::new(stream))
            .await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("H2 connection failed: {:?}", err);
            }
        });

        sender.send_request(forwarded).await?
    } else {
        let (mut sender, conn) = hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(TokioIo::new(stream))
            .await?;

        tokio::task::spawn(async move {
            if let Err(err) = conn.with_upgrades().await {
                println!("Connection failed: {:?}", err);
            }
        });

        sender.send_request(forwarded).await?
    };

    if response.status() == http::StatusCode::SWITCHING_PROTOCOLS {
        if let Some(client_upgrade) = maybe_client_upgrade {
            let server_upgrade = hyper::upgrade::on(&mut response);
            let (parts, server_body) = response.into_parts();

            tokio::task::spawn(tunnel(client_upgrade, server_upgrade, server_body));

            let response = Response::from_parts(parts, crate::http::body::empty());
            return Ok(
                ProxyResponse::new(response, response_headers.clone(), received_by.clone())
                    .into_forwarded(),
            );
        }

        // Upstream server sent us an HTTP 101 response without the client
        // asking for an upgrade, so we can't proxy data from the client.
        return Ok(LocalResponse::bad_gateway());
    }

    if should_cache && response.status().is_success() {
        let (parts, body) = response.into_parts();
        let body = body.collect().await?.to_bytes();
        put_cached_response(&cache_key, &parts, body.clone(), &cache).await;
        let response = Response::from_parts(parts, crate::http::body::full(body));
        return Ok(
            ProxyResponse::new(response, response_headers.clone(), received_by.clone())
                .into_forwarded(),
        );
    }

    Ok(ProxyResponse::new(
        response.map(|body| body.boxed()),
        response_headers,
        received_by,
    )
    .into_forwarded())
}

async fn get_cached_response(cache_key: &str) -> Option<BoxBodyResponse> {
    let mut store = cache_store().write().await;
    let entry = store.get(cache_key)?.clone();

    if Instant::now() > entry.expires_at {
        store.remove(cache_key);
        return None;
    }

    let mut response = Response::builder()
        .status(entry.status)
        .version(entry.version);
    *response.headers_mut().unwrap() = entry.headers;
    Some(response.body(crate::http::body::full(entry.body)).unwrap())
}

async fn put_cached_response(
    cache_key: &str,
    parts: &http::response::Parts,
    body: Bytes,
    cache: &config::Cache,
) {
    let mut store = cache_store().write().await;

    if store.len() >= cache.max_entries
        && let Some(first) = store.keys().next().cloned()
    {
        store.remove(&first);
    }

    let ttl = Duration::from_secs(cache.ttl_seconds.max(1));

    store.insert(cache_key.to_string(), CacheEntry {
        status: parts.status,
        version: parts.version,
        headers: parts.headers.clone(),
        body,
        expires_at: Instant::now() + ttl,
    });
}

/// TCP tunnel for upgraded connections such as Websockets or any other custom
/// protocol. This future should be spawned in a [`tokio::task`] as the client's
/// [`hyper::upgrade::Upgraded`] connection won't resolve until we send an
/// `HTTP 101` response back to the client.
async fn tunnel(client: OnUpgrade, server: OnUpgrade, _server_body: Incoming) {
    let upgraded_client = match client.await {
        Ok(upgraded_client) => upgraded_client,
        Err(_) => return,
    };

    let upgraded_server = match server.await {
        Ok(upgraded_server) => upgraded_server,
        Err(_) => return,
    };

    let mut upgraded_client = TokioIo::new(upgraded_client);
    let mut upgraded_server = TokioIo::new(upgraded_server);

    match tokio::io::copy_bidirectional(&mut upgraded_client, &mut upgraded_server).await {
        Ok((client_bytes, server_bytes)) => {
            println!("Client wrote {client_bytes} bytes, server wrote {server_bytes} bytes")
        }
        Err(err) => eprintln!("Tunnel error: {err}"),
    }
}
