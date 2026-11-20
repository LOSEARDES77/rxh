//! Admin API: JSON over HTTP on a loopback address (`127.0.0.1:2019` by
//! default, like Caddy), on a unix socket only its owner can open, or on any
//! address with a token. The CLI uses it to inspect and change a running
//! daemon.
//!
//! | Method   | Path                          | Action                              |
//! |----------|-------------------------------|-------------------------------------|
//! | `GET`    | `/status`                     | Version, listeners and pools        |
//! | `GET`    | `/config`                     | Running configuration (JSON)        |
//! | `GET`    | `/config/files`               | Files on disk as entities, masked   |
//! | `GET`    | `/schema`                     | JSON schema of the configuration    |
//! | `PUT`    | `/config/files/{index}`       | Save the text of one file           |
//! | `POST`   | `/config/entities/{kind}`     | Save a new pool, server or section  |
//! | `PUT`    | `/config/entities/{kind}/{key}` | Save one pool, server or section  |
//! | `DELETE` | `/config/entities/{kind}/{key}` | Delete it from its file           |
//! | `GET`    | `/pools`, `/pools/{name}`     | Pool members and health             |
//! | `GET`    | `/reverse_proxy/upstreams`    | Caddy compatible upstream list      |
//! | `POST`   | `/pools/{name}/upstreams`     | Add `{"address", "weight"}`         |
//! | `DELETE` | `/pools/{name}/upstreams`     | Remove `{"address"}`                |
//! | `GET`    | `/metrics`                    | Prometheus metrics                  |
//! | `GET`    | `/certificates`               | Certificates served, with expiry    |
//! | `GET`    | `/dashboard`                  | Web UI over the routes              |
//! | `POST`   | `/certificates/reload`        | Read certificate files again        |
//! | `POST`   | `/reload`                     | Reload the configuration file       |
//! | `POST`   | `/load`                       | Run the configuration in the body   |
//! | `PATCH`  | `/config/upstream/{name}`     | Merge the body into one pool        |
//! | `PATCH`  | `/config/server/{name}`       | Merge the body into one server      |
//! | `POST`   | `/stop`                       | Graceful shutdown                   |
//!
//! `/load` takes a whole configuration and the `PATCH` routes a JSON merge
//! patch (RFC 7396), as JSON or TOML. Both validate and swap like a reload,
//! and neither writes the configuration file: the next reload or restart
//! goes back to it. The `/config/files` and `/config/entities` routes write
//! the file, then reload it; a change that is not valid or does not reload
//! leaves the file as it was. `/load` without a body reloads the file. A server is
//! addressed by `name`, else by its index.
//!
//! With `[admin] token` every request must carry `Authorization: Bearer
//! <token>`. Without it, TCP requests whose `Host` or `Origin` is not a
//! loopback name are rejected, so web pages cannot reach the API through DNS
//! rebinding.
//!
//! `/dashboard` is a static web UI (built from `ui/`, served with
//! `/dashboard/app.js` and `/dashboard/app.css`) that calls the routes from
//! the browser. It holds no data, so those three paths are served without
//! the token, which the page asks for and sends on its requests.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
#[cfg(unix)]
use std::path::Path;
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
use rxh_core::config::{AdminListen, Backend, Config};
use rxh_core::pool::{BackendState, PoolError};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use crate::change::{self, Change, Section};
use crate::config_api;
use crate::files;
use crate::master::ServerInfo;
use crate::registry::Registry;
use crate::server::{ShutdownState, State};
use crate::systemd;
use crate::tls::{self, CertificateInfo};

/// The web UI, built from `ui/` by `bun run build` and committed, so building
/// the crate needs no JavaScript tooling.
const UI_INDEX: &str = include_str!("../ui/dist/index.html");
const UI_SCRIPT: &str = include_str!("../ui/dist/app.js");
const UI_STYLE: &str = include_str!("../ui/dist/app.css");

/// Lets the web UI load its own script and style and call the API it came
/// from, and nothing else: no inline code, no other origin, no frames around it.
const DASHBOARD_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     connect-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'";

const MAX_BODY: usize = 64 * 1024;

/// Limit for the bodies that carry configuration.
pub(crate) const MAX_CONFIG_BODY: usize = 4 * 1024 * 1024;

/// Requests from the admin API to the daemon loop.
#[derive(Debug)]
pub(crate) enum Control {
    /// Reload the configuration file and report the outcome.
    Reload(oneshot::Sender<Result<(), String>>),

    /// Run a changed configuration and report the outcome.
    Apply(Change, oneshot::Sender<Result<(), String>>),

    /// Start the binary on disk, hand it the listening sockets and report
    /// whether it took over.
    Upgrade(oneshot::Sender<Result<(), String>>),

    /// Shut down gracefully.
    Stop,
}

/// What is running right now.
pub(crate) struct Current {
    pub(crate) config: Arc<Config>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) servers: Vec<ServerInfo>,
}

impl Current {
    /// Running configuration with live pool membership.
    pub(crate) fn live_config(&self) -> Config {
        let mut config = Config::clone(&self.config);
        for (name, group) in &mut config.upstreams {
            if let Some(live) = self.registry.get(name) {
                *group = live.pool.to_group();
            }
        }
        config
    }
}

/// State shared by admin requests.
pub(crate) struct AdminState {
    pub(crate) started: Instant,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) listen: AdminListen,
    pub(crate) token: Option<String>,
    pub(crate) current: RwLock<Current>,
    pub(crate) control: mpsc::Sender<Control>,
    /// Held while a configuration file is saved and reloaded, so two saves
    /// do not write over each other.
    pub(crate) saving: tokio::sync::Mutex<()>,
}

#[derive(Deserialize)]
struct UpstreamBody {
    address: String,
    #[serde(default = "one")]
    weight: u32,
}

#[derive(Deserialize)]
struct StateBody {
    address: String,
    state: BackendState,
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

/// Bound admin endpoint. A unix socket file is removed on drop, unless it
/// came from systemd, which owns it.
pub(crate) enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener, Option<PathBuf>),
}

impl Listener {
    /// Registers the socket for a hot binary upgrade.
    pub(crate) fn lease(&self) -> io::Result<crate::upgrade::Lease> {
        match self {
            Self::Tcp(listener) => crate::upgrade::lease(listener),
            #[cfg(unix)]
            Self::Unix(listener, _) => crate::upgrade::lease(listener),
        }
    }

    /// Binds `listen`, replacing the socket file a dead daemon left behind.
    pub(crate) async fn bind(listen: &AdminListen) -> io::Result<Self> {
        match listen {
            AdminListen::Tcp(address) => match systemd::tcp_listener(*address)? {
                Some(listener) => {
                    listener.set_nonblocking(true)?;
                    TcpListener::from_std(listener).map(Self::Tcp)
                }
                None => TcpListener::bind(address).await.map(Self::Tcp),
            },
            #[cfg(unix)]
            AdminListen::Unix(path) => bind_unix(path).await,
            #[cfg(not(unix))]
            AdminListen::Unix(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix sockets are not available on this system",
            )),
        }
    }
}

#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        // After a hot binary upgrade the file belongs to the new process.
        if let Self::Unix(_, Some(path)) = self
            && !crate::upgrade::handed_over()
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
async fn bind_unix(path: &Path) -> io::Result<Listener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    if let Some(listener) = systemd::unix_listener(path)? {
        listener.set_nonblocking(true)?;
        return Ok(Listener::Unix(UnixListener::from_std(listener)?, None));
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the path exists and is not a socket",
            ));
        }
        Ok(_) if tokio::net::UnixStream::connect(path).await.is_ok() => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "a process is listening on the socket",
            ));
        }
        Ok(_) | Err(_) => {}
    }

    // Bound under another name and renamed once it is private, so the socket
    // never accepts a connection from another user, not even between `bind`
    // and `chmod`. The rename also replaces a stale socket.
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(format!(".{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let listener = UnixListener::bind(&temporary)?;
    let private = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| std::fs::rename(&temporary, path));
    if let Err(err) = private {
        let _ = std::fs::remove_file(&temporary);
        return Err(err);
    }
    Ok(Listener::Unix(listener, Some(path.to_owned())))
}

/// Serves the admin API until the task is dropped.
pub(crate) async fn serve(listener: Listener, state: Arc<AdminState>) {
    crate::history::global().start(&state);
    loop {
        match &listener {
            Listener::Tcp(listener) => {
                if let Ok((stream, _)) = listener.accept().await {
                    serve_connection(stream, &state);
                }
            }
            #[cfg(unix)]
            Listener::Unix(listener, _) => {
                if let Ok((stream, _)) = listener.accept().await {
                    serve_connection(stream, &state);
                }
            }
        }
    }
}

fn serve_connection<S>(stream: S, state: &Arc<AdminState>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let state = Arc::clone(state);
    tokio::task::spawn(async move {
        let service = service_fn(move |request| handle(request, Arc::clone(&state)));
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<AdminState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let raw_path = request.uri().path();
    let is_dashboard = request.method() == Method::GET
        && matches!(
            raw_path.trim_matches('/'),
            "dashboard" | "dashboard/app.js" | "dashboard/app.css"
        );
    // The page calls the API with relative URLs, which only resolve to the
    // API routes from this exact path.
    let is_dashboard_page = raw_path == "/dashboard";
    if let Some(token) = &state.token {
        // A browser cannot send the token when it opens a page.
        if !is_dashboard && !has_token(&request, token) {
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                "admin API requires Authorization: Bearer <token>",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
            return Ok(response);
        }
    } else if let AdminListen::Tcp(listen) = state.listen
        && !is_local_request(&request, listen)
    {
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
        .map(files::percent_decode)
        .collect();
    let segments: Vec<&str> = decoded.iter().map(String::as_str).collect();

    let response = match (&method, segments.as_slice()) {
        (&Method::GET, [] | ["status"]) => json_response(StatusCode::OK, &status(&state)),
        (&Method::GET, ["metrics"]) => {
            let mut response = Response::new(Full::new(Bytes::from(
                crate::metrics::global().render(&registry(&state)),
            )));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            response
        }
        (&Method::GET, ["stats", "history"]) => json_response(
            StatusCode::OK,
            &json!({
                "interval_seconds": crate::history::INTERVAL.as_secs(),
                "buckets": crate::metrics::BUCKETS,
                "samples": crate::history::global().samples(),
            }),
        ),
        (&Method::GET, ["dashboard"]) if is_dashboard_page => {
            dashboard(UI_INDEX, "text/html; charset=utf-8")
        }
        (&Method::GET, ["dashboard"]) => dashboard_redirect(),
        (&Method::GET, ["dashboard", "app.js"]) => {
            dashboard(UI_SCRIPT, "text/javascript; charset=utf-8")
        }
        (&Method::GET, ["dashboard", "app.css"]) => dashboard(UI_STYLE, "text/css; charset=utf-8"),
        (&Method::GET, ["config"]) => json_response(StatusCode::OK, &live_config(&state)),
        (&Method::GET, ["config", "files"]) => config_api::files(&state).await,
        (&Method::GET, ["schema"]) => config_api::schema(),
        (&Method::PUT, ["config", "files", index]) => {
            config_api::put_file(&state, index, request).await
        }
        (&Method::POST, ["config", "entities", kind]) => {
            config_api::create(&state, kind, request).await
        }
        (&Method::PUT, ["config", "entities", kind, key]) => {
            config_api::replace(&state, kind, key, request).await
        }
        (&Method::DELETE, ["config", "entities", kind, key]) => {
            let query = request.uri().query();
            config_api::remove(&state, kind, key, query).await
        }
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
        (&Method::POST | &Method::PUT, ["pools", name, "upstreams", "state"]) => {
            let name = (*name).to_owned();
            match read_json::<StateBody>(request).await {
                Ok(body) => set_state(&state, &name, &body),
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
        (&Method::POST, ["reload"]) => control(&state, Control::Reload, "reloaded").await,
        (&Method::POST, ["load"]) => match read_body(request, MAX_CONFIG_BODY).await {
            // Without a body `/load` is the `/reload` it was before uploads.
            Ok(body) if body.trim_ascii().is_empty() => {
                control(&state, Control::Reload, "reloaded").await
            }
            Ok(body) => match change::parse::<Config>(&body) {
                Ok(config) => {
                    let change = Change::Load(Box::new(config));
                    control(&state, |reply| Control::Apply(change, reply), "loaded").await
                }
                Err(message) => error(StatusCode::BAD_REQUEST, &message),
            },
            Err(response) => *response,
        },
        (&Method::PATCH, ["config", section, key]) if Section::from_segment(section).is_some() => {
            let section = Section::from_segment(section).expect("checked by the guard");
            let key = (*key).to_owned();
            patch(&state, section, key, request).await
        }
        (&Method::GET, ["certificates"]) => json_response(StatusCode::OK, &certificates(&state)),
        (&Method::POST, ["certificates", "reload"]) => reload_certificates(&state),
        (&Method::POST, ["upgrade"]) => control(&state, Control::Upgrade, "upgraded").await,
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

/// Sends a request to the daemon loop and answers with its outcome:
/// `{"<done>": true}`, or `400` with the reason it was rejected.
async fn control(
    state: &AdminState,
    request: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Control,
    done: &str,
) -> Response<Full<Bytes>> {
    match self::request(state, request).await {
        Ok(()) => json_response(StatusCode::OK, &json!({ done: true })),
        Err((status, message)) => error(status, &message),
    }
}

/// Sends a request to the daemon loop and waits for its outcome: `400` with
/// the reason it was rejected, `503` when the daemon is stopping.
pub(crate) async fn request(
    state: &AdminState,
    request: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Control,
) -> Result<(), (StatusCode, String)> {
    let stopping = || {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            String::from("daemon is shutting down"),
        )
    };
    let (reply, outcome) = oneshot::channel();
    if state.control.send(request(reply)).await.is_err() {
        return Err(stopping());
    }
    match outcome.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err((StatusCode::BAD_REQUEST, message)),
        Err(_) => Err(stopping()),
    }
}

async fn patch(
    state: &AdminState,
    section: Section,
    key: String,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let known = {
        let current = state.current.read().unwrap_or_else(PoisonError::into_inner);
        change::exists(&current.config, section, &key)
    };
    if !known {
        return error(StatusCode::NOT_FOUND, &change::missing(section, &key));
    }
    let body = match read_body(request, MAX_CONFIG_BODY).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let patch = match change::parse::<serde_json::Value>(&body) {
        Ok(patch) if patch.is_object() => patch,
        Ok(_) => return error(StatusCode::BAD_REQUEST, "the patch must be an object"),
        Err(message) => return error(StatusCode::BAD_REQUEST, &message),
    };
    let change = Change::Patch {
        section,
        key,
        patch,
    };
    control(state, |reply| Control::Apply(change, reply), "patched").await
}

/// The registry running now.
pub(crate) fn registry(state: &AdminState) -> Arc<Registry> {
    Arc::clone(
        &state
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .registry,
    )
}

#[derive(Serialize)]
struct ListenerCertificate {
    listen: SocketAddr,
    #[serde(flatten)]
    certificate: CertificateInfo,
}

/// Certificates of every TLS listener, soonest expiry first.
fn certificates(state: &AdminState) -> Vec<ListenerCertificate> {
    let current = state.current.read().unwrap_or_else(PoisonError::into_inner);
    let mut certificates: Vec<ListenerCertificate> = Vec::new();
    for server in &current.servers {
        let Some(resolver) = &server.cert_resolver else {
            continue;
        };
        for certificate in resolver.certificates() {
            certificates.push(ListenerCertificate {
                listen: server.address,
                certificate,
            });
        }
    }
    certificates.sort_by(|a, b| a.certificate.not_after.cmp(&b.certificate.not_after));
    certificates
}

/// Reads every certificate file again, changed or not. Files that fail
/// keep their previous certificate, and make the answer an error.
fn reload_certificates(state: &AdminState) -> Response<Full<Bytes>> {
    let resolvers: Vec<_> = state
        .current
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .servers
        .iter()
        .filter_map(|server| server.cert_resolver.clone())
        .collect();
    let mut reloaded = Vec::new();
    let mut errors = Vec::new();
    for resolver in resolvers {
        let outcomes = resolver.reload_files(/* force= */ true);
        tls::log_reloads(&outcomes);
        for (cert, outcome) in outcomes {
            match outcome {
                Ok(()) => reloaded.push(cert),
                Err(err) => errors.push(err),
            }
        }
    }
    if errors.is_empty() {
        json_response(StatusCode::OK, &json!({ "reloaded": reloaded }))
    } else {
        error(StatusCode::BAD_REQUEST, &errors.join("; "))
    }
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

fn set_state(state: &AdminState, name: &str, body: &StateBody) -> Response<Full<Bytes>> {
    let Some(group) = registry(state).get(name) else {
        return error(StatusCode::NOT_FOUND, &format!("no pool named {name}"));
    };
    if group.pool.set_state(&body.address, body.state) {
        println!("Admin => {} of {name} is now {}", body.address, body.state);
        json_response(StatusCode::OK, &group.pool.status())
    } else {
        error(
            StatusCode::NOT_FOUND,
            &format!("{} is not in pool {name}", body.address),
        )
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
        "admin": state.listen.to_string(),
        "servers": servers,
        "pools": pools,
    })
}

/// Running configuration with live pool membership and without the secrets
/// of the DNS provider.
fn live_config(state: &AdminState) -> Config {
    let mut config = state
        .current
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .live_config();
    if let Some(dns) = &mut config.acme.dns {
        dns.redact();
    }
    config.admin.redact();
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

/// True if the request carries `Authorization: Bearer <token>`. The
/// comparison takes the same time wherever the first difference is.
fn has_token<B>(request: &Request<B>, token: &str) -> bool {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .is_some_and(|(scheme, given)| {
            scheme.eq_ignore_ascii_case("bearer")
                && aws_lc_rs::constant_time::verify_slices_are_equal(
                    given.trim().as_bytes(),
                    token.as_bytes(),
                )
                .is_ok()
        })
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
    let body = read_body(request, MAX_BODY).await?;
    serde_json::from_slice(&body).map_err(|err| {
        Box::new(error(
            StatusCode::BAD_REQUEST,
            &format!("invalid JSON body: {err}"),
        ))
    })
}

pub(crate) async fn read_body(
    request: Request<Incoming>,
    limit: usize,
) -> Result<Bytes, Box<Response<Full<Bytes>>>> {
    let body = Limited::new(request.into_body(), limit)
        .collect()
        .await
        .map_err(|err| {
            Box::new(error(
                StatusCode::BAD_REQUEST,
                &format!("could not read body: {err}"),
            ))
        })?;
    Ok(body.to_bytes())
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
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

/// One file of the web UI.
fn dashboard(body: &'static str, content_type: &'static str) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from_static(body.as_bytes())));
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, content_type),
        (header::CONTENT_SECURITY_POLICY, DASHBOARD_POLICY),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CACHE_CONTROL, "no-cache"),
    ] {
        headers.insert(name, header::HeaderValue::from_static(value));
    }
    response
}

/// Sends `/dashboard/` and the like to the one path the page works from.
fn dashboard_redirect() -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::PERMANENT_REDIRECT;
    response.headers_mut().insert(
        header::LOCATION,
        header::HeaderValue::from_static("/dashboard"),
    );
    response
}

pub(crate) fn error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
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
        assert_eq!(files::percent_decode("server%5B0%5D%231"), "server[0]#1");
        assert_eq!(files::percent_decode("100%"), "100%");
    }

    #[test]
    fn the_front_end_builds_no_markup_from_strings() {
        let mut directories = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui/src")];
        let mut files = 0;
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Markup from strings can run injected script, and `style=`
                // is refused by the `style-src 'self'` policy.
                for banned in ["innerHTML", "{@html", "style="] {
                    assert!(!text.contains(banned), "{} holds {banned}", path.display());
                }
                files += 1;
            }
        }
        assert!(files > 0, "the walk must reach the sources to check them");
    }

    #[test]
    fn the_built_page_runs_no_inline_script() {
        // The policy only lets scripts and styles of the same origin run.
        assert_eq!(UI_INDEX.matches("<script").count(), 1);
        assert!(
            UI_INDEX.contains("<script type=\"module\" crossorigin src=\"./dashboard/app.js\">")
        );
        assert!(!UI_INDEX.contains("<style"));
        assert!(!UI_INDEX.contains("style="));
    }

    #[test]
    fn bearer_token_must_match_exactly() {
        let with = |value: &str| {
            Request::builder()
                .header(header::AUTHORIZATION, value)
                .body(())
                .unwrap()
        };
        let token = "0123456789abcdef";
        assert!(has_token(&with("Bearer 0123456789abcdef"), token));
        assert!(has_token(&with("bearer 0123456789abcdef"), token));
        assert!(!has_token(&with("Bearer 0123456789abcdeX"), token));
        assert!(!has_token(&with("Bearer 0123456789abcde"), token));
        assert!(!has_token(&with("Bearer 0123456789abcdef0"), token));
        assert!(!has_token(&with("Bearer"), token));
        assert!(!has_token(&with("Basic 0123456789abcdef"), token));
        assert!(!has_token(&with("0123456789abcdef"), token));
        assert!(!has_token(&request("localhost", None), token));
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
