//! Reverse proxy handler: picks a backend, rewrites the request, streams the
//! response back and feeds passive health checks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::Version;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use rxh_core::address::BackendAddress;
use rxh_core::pool::{RequestGuard, SelectContext, Upstream};

use crate::http::body::{self, Guarded};
use crate::http::cache::Cache;
use crate::http::headers::{self, Forwarding};
use crate::http::response::{self, BoxResponse};
use crate::http::service::{Route, Service};
use crate::http::transport::SendError;
use crate::registry::Group;
use crate::upstream::{self, DialError};

/// Pooled connections found closed before a request is written are
/// replaced this many times per request.
const MAX_STALE_RETRIES: usize = 3;

/// Proxies `request` to a backend of `group`.
pub(crate) async fn forward(
    mut request: Request<Incoming>,
    route: &Route,
    group: &Group,
    connection: &Service,
) -> BoxResponse {
    let site = &route.site;
    let (client_ip, trusted) = headers::client_ip(
        connection.peer.ip(),
        request.headers(),
        &site.trusted_proxies,
    );
    let h2c = group.transport.is_h2c();
    let upgrade = !h2c
        && request.version() != Version::HTTP_2
        && headers::requested_upgrade(request.headers()).is_some();

    let cache_key = route
        .cache
        .as_ref()
        .filter(|_| request.method() == Method::GET && !upgrade)
        .map(|_| format!("{}|{}|{}", group.pool.name(), host(&request), request.uri()));

    if let (Some(cache), Some(key)) = (&route.cache, &cache_key)
        && let Some(cached) = cache.get(key)
    {
        return cached;
    }

    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));

    let (mut parts, incoming) = request.into_parts();
    let forwarding = Forwarding {
        peer: connection.peer,
        local: connection.local,
        trusted,
        tls: site.tls,
        by: &site.by,
        rules: &route.request,
    };
    headers::prepare_request(
        &mut parts.headers,
        parts.version,
        &parts.uri,
        client_ip,
        &forwarding,
    );

    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    if h2c {
        parts.headers.remove(header::CONNECTION);
        parts.headers.remove(header::UPGRADE);
        let host = parts
            .headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("localhost");
        parts.uri = format!("http://{host}{path}")
            .parse()
            .unwrap_or_else(|_| Uri::from_static("/"));
        parts.version = Version::HTTP_2;
    } else {
        parts.uri = path.parse().unwrap_or_else(|_| Uri::from_static("/"));
        parts.version = Version::HTTP_11;
    }

    let outgoing = if client_upgrade.is_some() {
        body::empty()
    } else {
        incoming.boxed()
    };
    let mut outgoing = Request::from_parts(parts, outgoing);
    let transport = &group.transport;
    let mut stale_attempts = 0;
    // Like Caddy, HTTPS backends get their own host as `Host` unless the user sets it.
    let rewrite_host = group.dialer.is_tls()
        && !route
            .request
            .set
            .keys()
            .any(|name| name.eq_ignore_ascii_case("host"));

    // Like Caddy, requests without a body and with a safe method are retried
    // on another backend when the round trip fails after connecting.
    let replay = (client_upgrade.is_none()
        && matches!(
            *outgoing.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        )
        && hyper::body::Body::is_end_stream(outgoing.body()))
    .then(|| head(&outgoing));
    let mut round_trip_retries = group.pool.settings().retries;
    if group.pool.settings().try_duration.is_some() {
        round_trip_retries = round_trip_retries.max(1);
    }

    // Loops only when a pooled connection turned out to be closed before the
    // request was written, so retrying is safe for any method.
    let (selection, guard, started, mut upstream_response) = loop {
        let context = SelectContext {
            peer_ip: connection.peer.ip(),
            client_ip,
            uri: Some(outgoing.uri()),
            headers: Some(outgoing.headers()),
            secure: site.tls,
        };

        let dialed = upstream::connect_with_retries(
            &group.pool,
            &context,
            |upstream: Arc<Upstream>| async move { transport.connect(upstream.address()).await },
        )
        .await;

        let (selection, upstream_connection) = match dialed {
            Ok(dialed) => dialed,
            Err(DialError::NoUpstream) => return response::service_unavailable(),
            Err(DialError::Connect(_)) => return response::bad_gateway(),
        };

        let guard = selection.upstream.start_request();
        let address = selection.upstream.address();
        if rewrite_host {
            set_backend_host(&mut outgoing, address);
        }
        let started = Instant::now();

        let sent = transport.send(address, upstream_connection, outgoing);
        let sent = match group.pool.settings().response_header_timeout {
            Some(limit) => match tokio::time::timeout(limit, sent).await {
                Ok(sent) => sent,
                Err(_) => {
                    selection.upstream.record_failure();
                    eprintln!(
                        "{} => upstream {address} sent no response headers within {limit:?}",
                        site.log_name
                    );
                    return response::gateway_timeout();
                }
            },
            None => sent.await,
        };

        match sent {
            Ok(upstream_response) => break (selection, guard, started, upstream_response),
            Err(SendError::Stale(request)) if stale_attempts < MAX_STALE_RETRIES => {
                stale_attempts += 1;
                transport.forget(address);
                outgoing = *request;
            }
            Err(SendError::Stale(_)) => {
                eprintln!(
                    "{} => upstream {address} keeps closing connections",
                    site.log_name
                );
                return response::bad_gateway();
            }
            Err(SendError::Failed(err)) => {
                selection.upstream.record_failure();
                eprintln!("{} => upstream {address} failed: {err}", site.log_name);
                match &replay {
                    Some(head) if round_trip_retries > 0 => {
                        round_trip_retries -= 1;
                        outgoing = rebuild(head);
                    }
                    _ => return response::bad_gateway(),
                }
            }
        }
    };
    selection
        .upstream
        .record_response(upstream_response.status().as_u16(), started.elapsed());

    if upstream_response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade else {
            // The backend switched protocols without being asked to.
            return response::bad_gateway();
        };
        let server_upgrade = hyper::upgrade::on(&mut upstream_response);
        tokio::task::spawn(tunnel(
            client_upgrade,
            server_upgrade,
            guard,
            site.stream_timeout,
        ));

        let (parts, _) = upstream_response.into_parts();
        let mut client_response = Response::from_parts(parts, body::empty());
        headers::prepare_response(&mut client_response, &route.response, &site.by);
        return client_response;
    }

    let mut client_response = upstream_response.map(BodyExt::boxed);
    headers::prepare_response(&mut client_response, &route.response, &site.by);

    if let (Some(cache), Some(key)) = (&route.cache, cache_key) {
        client_response = store(cache, key, client_response).await;
    }

    if let Some(cookie) = selection.set_cookie
        && let Ok(cookie) = HeaderValue::from_str(&cookie)
    {
        client_response
            .headers_mut()
            .append(header::SET_COOKIE, cookie);
    }

    client_response.map(|body| Guarded::new(body, guard).boxed())
}

/// Buffers and caches the response when it is small and storable.
async fn store(cache: &Cache, key: String, response: BoxResponse) -> BoxResponse {
    let (parts, body) = response.into_parts();

    let declared_len = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    let fits = declared_len.is_some_and(|len| len <= cache.max_body_bytes());

    if !Cache::is_storable(&parts) || !fits {
        return Response::from_parts(parts, body);
    }

    match Limited::new(body, cache.max_body_bytes()).collect().await {
        Ok(collected) => {
            let bytes: Bytes = collected.to_bytes();
            cache.put(key, &parts, bytes.clone());
            Response::from_parts(parts, body::full(bytes))
        }
        Err(_) => response::bad_gateway(),
    }
}

/// Copy of a request head, to send it again.
fn head<B>(request: &Request<B>) -> Request<()> {
    let mut copy = Request::new(());
    *copy.method_mut() = request.method().clone();
    *copy.uri_mut() = request.uri().clone();
    *copy.version_mut() = request.version();
    *copy.headers_mut() = request.headers().clone();
    copy
}

/// Bodyless request built from a [`head`] copy.
fn rebuild(head: &Request<()>) -> Request<body::Body> {
    let mut request = Request::new(body::empty());
    *request.method_mut() = head.method().clone();
    *request.uri_mut() = head.uri().clone();
    *request.version_mut() = head.version();
    *request.headers_mut() = head.headers().clone();
    request
}

/// Points `Host` (and the authority of an absolute URI) at the backend.
fn set_backend_host<B>(request: &mut Request<B>, address: &str) {
    let Ok(BackendAddress::Tcp { host, port, .. }) = BackendAddress::parse(address) else {
        return;
    };
    let authority = if port == 443 {
        host.to_owned()
    } else {
        format!("{host}:{port}")
    };
    let Ok(value) = HeaderValue::from_str(&authority) else {
        return;
    };
    request.headers_mut().insert(header::HOST, value);

    if request.uri().authority().is_some() {
        let mut parts = request.uri().clone().into_parts();
        parts.authority = authority.parse().ok();
        if let Ok(uri) = Uri::from_parts(parts) {
            *request.uri_mut() = uri;
        }
    }
}

fn host<B>(request: &Request<B>) -> String {
    request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| {
            request
                .uri()
                .authority()
                .map(|authority| authority.as_str())
        })
        .unwrap_or_default()
        .to_owned()
}

/// Copies bytes both ways between an upgraded client and backend (WebSocket
/// or any other protocol). Runs until either side closes or `stream_timeout`
/// passes.
async fn tunnel(
    client: OnUpgrade,
    server: OnUpgrade,
    _guard: RequestGuard,
    stream_timeout: Option<Duration>,
) {
    let (Ok(client), Ok(server)) = (client.await, server.await) else {
        return;
    };

    let mut client = TokioIo::new(client);
    let mut server = TokioIo::new(server);

    let copy = tokio::io::copy_bidirectional(&mut client, &mut server);
    let result = match stream_timeout {
        Some(limit) => match tokio::time::timeout(limit, copy).await {
            Ok(result) => result,
            Err(_) => return,
        },
        None => copy.await,
    };
    if let Err(err) = result {
        eprintln!("tunnel error: {err}");
    }
}
