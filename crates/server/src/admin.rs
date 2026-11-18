//! Admin API: JSON over HTTP on a loopback address (`127.0.0.1:2019` by
//! default, like Caddy). The CLI uses it to inspect and change a running
//! daemon.
//!
//! | Method   | Path                          | Action                              |
//! |----------|-------------------------------|-------------------------------------|
//! | `GET`    | `/status`                     | Version, listeners and pools        |
//! | `GET`    | `/config`                     | Running configuration (JSON)        |
//! | `GET`    | `/pools`, `/pools/{name}`     | Pool members and health             |
//! | `GET`    | `/reverse_proxy/upstreams`    | Caddy compatible upstream list      |
//! | `POST`   | `/pools/{name}/upstreams`     | Add `{"address", "weight"}`         |
//! | `DELETE` | `/pools/{name}/upstreams`     | Remove `{"address"}`                |
//! | `POST`   | `/reload`                     | Reload the configuration file       |
//! | `POST`   | `/stop`                       | Graceful shutdown                   |
//!
//! Requests whose `Host` or `Origin` is not a loopback name are rejected, so
//! web pages cannot reach the API through DNS rebinding.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Method;
use hyper::Request;
use hyper::Response;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::header;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rxh_core::config::{Backend, Config};
use rxh_core::pool::PoolError;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::master::ServerInfo;
use crate::registry::Registry;
use crate::server::{ShutdownState, State};

const MAX_BODY: usize = 64 * 1024;

/// Requests from the admin API to the daemon loop.
#[derive(Debug)]
pub(crate) enum Control {
    /// Reload the configuration file and report the outcome.
    Reload(oneshot::Sender<Result<(), String>>),

    /// Shut down gracefully.
    Stop,
}

/// What is running right now.
pub(crate) struct Current {
    pub(crate) config: Arc<Config>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) servers: Vec<ServerInfo>,
}

/// State shared by admin requests.
pub(crate) struct AdminState {
    pub(crate) started: Instant,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) listen: SocketAddr,
    pub(crate) current: RwLock<Current>,
    pub(crate) control: mpsc::Sender<Control>,
}

#[derive(Deserialize)]
struct UpstreamBody {
    address: String,
    #[serde(default = "one")]
    weight: u32,
}

fn one() -> u32 {
    1
}

#[derive(Serialize)]
struct ServerStatus {
    listen: SocketAddr,
    protocol: String,
    name: Option<String>,
    state: String,
}

/// Serves the admin API until the task is dropped.
pub(crate) async fn serve(listener: TcpListener, state: Arc<AdminState>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let state = Arc::clone(&state);
        tokio::task::spawn(async move {
            let service = service_fn(move |request| handle(request, Arc::clone(&state)));
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<AdminState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if !is_local_request(&request, state.listen) {
        return Ok(error(
            StatusCode::FORBIDDEN,
            "admin API only accepts loopback Host and Origin",
        ));
    }

    let method = request.method().clone();
    let path = request.uri().path().trim_matches('/').to_owned();
    let decoded: Vec<String> = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(percent_decode)
        .collect();
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();

    let response = match (&method, segments.as_slice()) {
        (&Method::GET, [] | ["status"]) => json_response(StatusCode::OK, &status(&state)),
        (&Method::GET, ["config"]) => json_response(StatusCode::OK, &live_config(&state)),
        (&Method::GET, ["pools"]) => {
            let registry = registry(&state);
            let pools: Vec<_> = registry.groups().map(|group| group.pool.status()).collect();
            json_response(StatusCode::OK, &pools)
        }
        (&Method::GET, ["pools", name]) => match registry(&state).get(name) {
            Some(group) => json_response(StatusCode::OK, &group.pool.status()),
            None => error(StatusCode::NOT_FOUND, &format!("no pool named {name}")),
        },
        (&Method::GET, ["reverse_proxy", "upstreams"]) => {
            let upstreams: Vec<_> = registry(&state)
                .groups()
                .flat_map(|group| group.pool.upstreams())
                .map(|upstream| {
                    let status = upstream.status();
                    json!({ "address": status.address, "num_requests": status.num_requests, "fails": status.fails })
                })
                .collect();
            json_response(StatusCode::OK, &upstreams)
        }
        (&Method::POST | &Method::PUT, ["pools", name, "upstreams"]) => {
            let name = (*name).to_owned();
            match read_json::<UpstreamBody>(request).await {
                Ok(body) => add_upstream(&state, &name, body),
                Err(response) => *response,
            }
        }
        (&Method::DELETE, ["pools", name, "upstreams"]) => {
            let name = (*name).to_owned();
            let query_address = request.uri().query().and_then(|query| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("address=").map(str::to_owned))
            });
            let address = match query_address {
                Some(address) => Ok(address),
                None => read_json::<UpstreamBody>(request)
                    .await
                    .map(|body| body.address),
            };
            match address {
                Ok(address) => remove_upstream(&state, &name, &address),
                Err(response) => *response,
            }
        }
        (&Method::POST, ["reload" | "load"]) => {
            let (reply, outcome) = oneshot::channel();
            if state.control.send(Control::Reload(reply)).await.is_err() {
                error(StatusCode::SERVICE_UNAVAILABLE, "daemon is shutting down")
            } else {
                match outcome.await {
                    Ok(Ok(())) => json_response(StatusCode::OK, &json!({ "reloaded": true })),
                    Ok(Err(message)) => error(StatusCode::BAD_REQUEST, &message),
                    Err(_) => error(StatusCode::SERVICE_UNAVAILABLE, "daemon is shutting down"),
                }
            }
        }
        (&Method::POST, ["stop"]) => {
            let _ = state.control.send(Control::Stop).await;
            json_response(StatusCode::OK, &json!({ "stopping": true }))
        }
        _ => error(
            StatusCode::NOT_FOUND,
            &format!("no route for {method} /{path}"),
        ),
    };

    Ok(response)
}

fn registry(state: &AdminState) -> Arc<Registry> {
    Arc::clone(
        &state
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .registry,
    )
}

fn add_upstream(state: &AdminState, name: &str, body: UpstreamBody) -> Response<Full<Bytes>> {
    let Some(group) = registry(state).get(name) else {
        return error(StatusCode::NOT_FOUND, &format!("no pool named {name}"));
    };
    match group.pool.add(Backend {
        address: body.address.clone(),
        weight: body.weight,
    }) {
        Ok(()) => {
            println!("Admin => added {} to {name}", body.address);
            json_response(StatusCode::CREATED, &group.pool.status())
        }
        Err(err @ PoolError::InvalidAddress(_)) => error(StatusCode::BAD_REQUEST, &err.to_string()),
        Err(err) => error(StatusCode::CONFLICT, &err.to_string()),
    }
}

fn remove_upstream(state: &AdminState, name: &str, address: &str) -> Response<Full<Bytes>> {
    let Some(group) = registry(state).get(name) else {
        return error(StatusCode::NOT_FOUND, &format!("no pool named {name}"));
    };
    if group.pool.remove(address) {
        println!("Admin => removed {address} from {name}");
        json_response(StatusCode::OK, &group.pool.status())
    } else {
        error(
            StatusCode::NOT_FOUND,
            &format!("{address} is not in pool {name}"),
        )
    }
}

fn status(state: &AdminState) -> serde_json::Value {
    let current = state.current.read().unwrap_or_else(PoisonError::into_inner);
    let servers: Vec<ServerStatus> = current
        .servers
        .iter()
        .map(|server| ServerStatus {
            listen: server.address,
            protocol: server.protocol.to_string(),
            name: server.name.clone(),
            state: state_name(*server.state.borrow()),
        })
        .collect();
    let pools: Vec<_> = current
        .registry
        .groups()
        .map(|group| group.pool.status())
        .collect();

    json!({
        "version": crate::VERSION,
        "pid": std::process::id(),
        "uptime_seconds": state.started.elapsed().as_secs(),
        "config_path": state.config_path,
        "admin": state.listen,
        "servers": servers,
        "pools": pools,
    })
}

/// Running configuration with live pool membership.
fn live_config(state: &AdminState) -> Config {
    let current = state.current.read().unwrap_or_else(PoisonError::into_inner);
    let mut config = Config::clone(&current.config);
    for (name, group) in &mut config.upstreams {
        if let Some(live) = current.registry.get(name) {
            *group = live.pool.to_group();
        }
    }
    config
}

fn state_name(state: State) -> String {
    match state {
        State::Starting => String::from("starting"),
        State::Listening => String::from("listening"),
        State::MaxConnectionsReached(max) => format!("max_connections_reached ({max})"),
        State::ShuttingDown(ShutdownState::PendingConnections(pending)) => {
            format!("shutting_down ({pending} pending)")
        }
        State::ShuttingDown(ShutdownState::Done) => String::from("stopped"),
    }
}

/// True if both `Host` and `Origin` (when present) name a loopback host or
/// the admin address itself.
fn is_local_request<B>(request: &Request<B>, listen: SocketAddr) -> bool {
    let allowed = |authority: &str| {
        let host = match authority.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        matches!(host, "localhost" | "127.0.0.1" | "::1") || host == listen.ip().to_string()
    };

    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_none_or(allowed);

    let origin_ok = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_none_or(|origin| {
            let authority = origin.split("://").nth(1).unwrap_or(origin);
            allowed(authority.trim_end_matches('/'))
        });

    host_ok && origin_ok
}

async fn read_json<T: for<'de> Deserialize<'de>>(
    request: Request<Incoming>,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let body = Limited::new(request.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|err| {
            Box::new(error(
                StatusCode::BAD_REQUEST,
                &format!("could not read body: {err}"),
            ))
        })?
        .to_bytes();
    serde_json::from_slice(&body).map_err(|err| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            &format!("invalid JSON body: {err}"),
        ))
    })
}

/// Decodes `%XX` escapes. Invalid escapes are kept as they are.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let escaped = (bytes[index] == b'%')
            .then(|| bytes.get(index + 1..index + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec_pretty(value).unwrap_or_else(|err| {
        format!("{{\"error\":\"serialization failed: {err}\"}}").into_bytes()
    });
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    json_response(status, &json!({ "error": message }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(host: &str, origin: Option<&str>) -> Request<()> {
        let mut builder = Request::builder().header(header::HOST, host);
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        builder.body(()).unwrap()
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("server%5B0%5D%231"), "server[0]#1");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn only_loopback_hosts_are_allowed() {
        let listen: SocketAddr = "127.0.0.1:2019".parse().unwrap();
        assert!(is_local_request(&request("localhost:2019", None), listen));
        assert!(is_local_request(
            &request("127.0.0.1:2019", Some("http://localhost:2019")),
            listen
        ));
        assert!(is_local_request(&request("[::1]:2019", None), listen));
        assert!(!is_local_request(
            &request("evil.example:2019", None),
            listen
        ));
        assert!(!is_local_request(
            &request("localhost:2019", Some("https://evil.example")),
            listen
        ));
    }
}
