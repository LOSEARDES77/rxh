//! Reverse proxy handler: picks a backend, rewrites the request, streams the
//! response back and feeds passive health checks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;
use rxh_core::address::BackendAddress;
use rxh_core::config::RetryMatch;
use rxh_core::pool::{RequestGuard, SelectContext, Upstream};
use tokio::sync::watch;

use crate::http::body::{self, BodyError, Guarded};
use crate::http::cache::Cache;
use crate::http::headers::{self, Forwarding};
use crate::http::response::{self, BoxResponse};
use crate::http::service::{Route, Service, UpstreamAddress};
use crate::http::transport::{Permit, SendError};
use crate::proxy_protocol;
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

    // What `handle_response` entries need from the request once it is gone.
    let handled = (!route.handle_response.is_empty()).then(|| (head(&request), host(&request)));

    let client_upgrade = upgrade.then(|| hyper::upgrade::on(&mut request));

    let (mut parts, incoming) = request.into_parts();
    let forwarding = Forwarding {
        peer: connection.peer,
        local: connection.local,
        trusted,
        tls: site.tls,
        by: &site.by,
        rules: &route.request,
        replace: &route.request_replace,
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

    let settings = group.pool.settings();
    // Set when the request body turns out to be larger than `max_body_size`.
    let too_large = Arc::new(AtomicBool::new(false));
    let mut outgoing = match route.max_body_size {
        _ if client_upgrade.is_some() => body::empty(),
        Some(limit) => {
            let too_large = Arc::clone(&too_large);
            Limited::new(incoming, limit)
                .map_err(move |err| match err.downcast::<hyper::Error>() {
                    Ok(err) => BodyError::Hyper(*err),
                    Err(_) => {
                        too_large.store(true, Ordering::Release);
                        BodyError::TooLarge
                    }
                })
                .boxed()
        }
        None => body::incoming(incoming),
    };
    if settings.request_buffers > 0 && client_upgrade.is_none() {
        outgoing = match body::buffer(outgoing, settings.request_buffers).await {
            Ok(buffered) => buffered,
            Err(_) if too_large.load(Ordering::Acquire) => return response::payload_too_large(),
            Err(err) => {
                eprintln!(
                    "{} => reading the request body failed: {err}",
                    site.log_name
                );
                return response::plain(StatusCode::BAD_REQUEST, "HTTP 400 BAD REQUEST");
            }
        };
    }
    let mut outgoing = Request::from_parts(parts, outgoing);
    let transport = &group.transport;
    let preface: Vec<u8> = site
        .proxy_protocol
        .map(|version| proxy_protocol::header(version, connection.peer, connection.local))
        .unwrap_or_default();
    let preface = &preface;
    let mut stale_attempts = 0;
    // Like Caddy, HTTPS backends get their own host as `Host` unless the user sets it.
    let rewrite_host = group.dialer.is_tls()
        && !route
            .request
            .set
            .keys()
            .any(|name| name.eq_ignore_ascii_case("host"));

    // Like Caddy, bodyless requests with a retryable method (safe methods
    // unless `retry_match.method` says otherwise) are retried on another
    // backend when the round trip fails after connecting, or when the
    // response status matches `retry_match.status`.
    let default_match = RetryMatch::default();
    let retry_match = settings.retry_match.as_ref().unwrap_or(&default_match);
    let replay = (client_upgrade.is_none()
        && retry_match.retries_method(outgoing.method())
        && hyper::body::Body::is_end_stream(outgoing.body()))
    .then(|| head(&outgoing));
    let mut round_trip_retries = settings.retries;
    if settings.try_duration.is_some() {
        round_trip_retries = round_trip_retries.max(1);
    }
    let first_try = Instant::now();
    let in_budget = |retries: u32| {
        retries > 0
            && settings
                .try_duration
                .is_none_or(|limit| first_try.elapsed() + settings.try_interval < limit)
    };

    // Loops only when a pooled connection turned out to be closed before the
    // request was written, so retrying is safe for any method.
    let (selection, guard, started, (mut upstream_response, permit)) = loop {
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
            |upstream: Arc<Upstream>| async move {
                transport.connect(upstream.address(), preface).await
            },
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

        let sent = transport.send(address, preface, upstream_connection, outgoing);
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
            Ok((unwanted, _))
                if replay.is_some()
                    && in_budget(round_trip_retries)
                    && retry_match.retries_status(unwanted.status().as_u16()) =>
            {
                // The response is dropped unread; another backend gets the request.
                selection
                    .upstream
                    .record_response(unwanted.status().as_u16(), started.elapsed());
                round_trip_retries -= 1;
                tokio::time::sleep(settings.try_interval).await;
                outgoing = rebuild(replay.as_ref().expect("guard checked replay"));
            }
            Ok(sent) => break (selection, guard, started, sent),
            Err(SendError::Stale(request)) if stale_attempts < MAX_STALE_RETRIES => {
                stale_attempts += 1;
                transport.forget(address, preface);
                outgoing = *request;
            }
            Err(SendError::Stale(_)) => {
                eprintln!(
                    "{} => upstream {address} keeps closing connections",
                    site.log_name
                );
                return response::bad_gateway();
            }
            // The backend is not to blame for a request cut short by the limit.
            Err(SendError::Failed(_)) if too_large.load(Ordering::Acquire) => {
                return response::payload_too_large();
            }
            Err(SendError::Failed(err)) => {
                selection.upstream.record_failure();
                eprintln!("{} => upstream {address} failed: {err}", site.log_name);
                match &replay {
                    Some(head) if in_budget(round_trip_retries) => {
                        round_trip_retries -= 1;
                        tokio::time::sleep(settings.try_interval).await;
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
            (guard, permit),
            site.stream_timeout,
            site.stream_close.clone(),
        ));

        let (parts, _) = upstream_response.into_parts();
        let mut client_response = Response::from_parts(parts, body::empty());
        headers::prepare_response(
            &mut client_response,
            &route.response,
            &route.response_replace,
            &site.by,
        );
        client_response
            .extensions_mut()
            .insert(UpstreamAddress(selection.upstream.address().to_owned()));
        return client_response;
    }

    let handler = route
        .handle_response
        .iter()
        .find(|handler| handler.matches(&upstream_response));
    if let (Some(handler), Some((request, host))) = (handler, &handled)
        && let Some(mut replaced) = handler.apply(request, host, &mut upstream_response).await
    {
        // The upstream response is dropped unread.
        replaced
            .extensions_mut()
            .insert(UpstreamAddress(selection.upstream.address().to_owned()));
        return replaced;
    }

    let mut client_response = upstream_response.map(body::incoming);
    if settings.response_buffers > 0 {
        let (parts, body) = client_response.into_parts();
        client_response = match body::buffer(body, settings.response_buffers).await {
            Ok(buffered) => Response::from_parts(parts, buffered),
            Err(err) => {
                selection.upstream.record_failure();
                eprintln!(
                    "{} => upstream {} failed: {err}",
                    site.log_name,
                    selection.upstream.address()
                );
                return response::bad_gateway();
            }
        };
    }
    if let Some(interval) = settings.flush_interval
        && batches(client_response.headers())
    {
        client_response = client_response.map(|body| body::Batched::new(body, interval).boxed());
    }
    headers::prepare_response(
        &mut client_response,
        &route.response,
        &route.response_replace,
        &site.by,
    );

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

    client_response
        .extensions_mut()
        .insert(UpstreamAddress(selection.upstream.address().to_owned()));
    client_response.map(|body| Guarded::new(body, (guard, permit)).boxed())
}

/// Whether `flush_interval` applies to a response with these headers. Like
/// Caddy, streams of unknown length and server-sent events are always
/// written as they come.
fn batches(headers: &HeaderMap) -> bool {
    let event_stream = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("text/event-stream"));
    headers.contains_key(header::CONTENT_LENGTH) && !event_stream
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
/// or any other protocol). Runs until either side closes, `stream_timeout`
/// passes, or `stream_close_delay` passes after the listener shut down.
async fn tunnel(
    client: OnUpgrade,
    server: OnUpgrade,
    _guard: (RequestGuard, Permit),
    stream_timeout: Option<Duration>,
    stream_close: Option<(Duration, watch::Receiver<bool>)>,
) {
    let (Ok(client), Ok(server)) = (client.await, server.await) else {
        return;
    };

    let mut client = TokioIo::new(client);
    let mut server = TokioIo::new(server);

    let copy = tokio::io::copy_bidirectional(&mut client, &mut server);
    let copy = async {
        match stream_timeout {
            Some(limit) => tokio::time::timeout(limit, copy).await.ok(),
            None => Some(copy.await),
        }
    };
    let closed = async {
        let Some((delay, mut closing)) = stream_close else {
            return std::future::pending().await;
        };
        // A dropped sender means the listener is gone too.
        let _ = closing.wait_for(|closing| *closing).await;
        tokio::time::sleep(delay).await;
    };
    let result = tokio::select! {
        result = copy => result,
        () = closed => None,
    };
    let Some(result) = result else {
        return;
    };
    if let Err(err) = result {
        eprintln!("tunnel error: {err}");
    }
}
