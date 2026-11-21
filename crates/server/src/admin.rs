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
//! | `GET`    | `/auth/session`               | Who the caller is                   |
//! | `POST`   | `/auth/login`, `/auth/logout` | Sign in and out with a session      |
//! | `POST`   | `/auth/setup`                 | Create the first user               |
//! | `PUT`    | `/auth/password`              | Change one's own password           |
//!
//! `/load` takes a whole configuration and the `PATCH` routes a JSON merge
//! patch (RFC 7396), as JSON or TOML. Both validate and swap like a reload,
//! and neither writes the configuration file: the next reload or restart
//! goes back to it. The `/config/files` and `/config/entities` routes write
//! the file, then reload it; a change that is not valid or does not reload
//! leaves the file as it was. `/load` without a body reloads the file. A server is
//! addressed by `name`, else by its index.
//!
//! Every request goes through one gate, which first works out who sends it
//! (see [`Caller`]): the holder of `[admin] token`, a signed-in user, a
//! local request when no token is set, or nobody. A request is local when
//! its connection comes from this machine (see [`connection_is_local`]) and
//! neither `Host` nor `Origin` is a name other than a loopback one, so web
//! pages cannot reach the API through DNS rebinding. The gate then lets the
//! request through only when [`ROUTES`] says the route is for that caller:
//! a route that is not listed there is for administrators.
//!
//! With `[admin] tls` the listener answers HTTPS, and plain HTTP to
//! connections from this machine, on the same port.
//!
//! `/dashboard` is a static web UI (built from `ui/`, served with
//! `/dashboard/app.js` and `/dashboard/app.css`) that calls the routes from
//! the browser. It holds no data, so those three paths are served without
//! the token, which the page asks for and sends on its requests.

use std::convert::Infallible;
use std::io;
use std::net::{IpAddr, SocketAddr};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

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
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

use crate::access::{Access, Scope, Store};
use crate::auth_api;
use crate::change::{self, Change, Section};
use crate::config_api;
use crate::files;
use crate::master::ServerInfo;
use crate::registry::Registry;
use crate::server::{ShutdownState, State};
use crate::session::{self, Session, Sessions};
use crate::systemd;
use crate::tls::{self, AdminTls, CertificateInfo};

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

/// Who a route of the admin API answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Audience {
    /// Everybody, signed in or not.
    Anyone,
    /// Whoever is signed in. The route itself checks what its caller may do.
    Caller,
    /// Administrators: the token, a local request without a token, and
    /// users with the `admin` role.
    Admin,
}

/// Every route of the admin API: method, path without its outer slashes
/// (`{}` is a segment that names something) and audience. The gate reads
/// the audience here, and what is not here is for administrators: a route
/// answers anybody else only because this table says so.
pub const ROUTES: &[(&str, &str, Audience)] = &[
    ("GET", "", Audience::Admin),
    ("GET", "status", Audience::Admin),
    ("GET", "metrics", Audience::Admin),
    ("GET", "stats/history", Audience::Admin),
    ("GET", "dashboard", Audience::Anyone),
    ("GET", "dashboard/app.js", Audience::Anyone),
    ("GET", "dashboard/app.css", Audience::Anyone),
    ("GET", "auth/session", Audience::Anyone),
    ("POST", "auth/login", Audience::Anyone),
    ("POST", "auth/setup", Audience::Anyone),
    ("POST", "auth/logout", Audience::Anyone),
    ("PUT", "auth/password", Audience::Caller),
    ("GET", "config", Audience::Admin),
    ("GET", "config/files", Audience::Admin),
    ("GET", "schema", Audience::Admin),
    ("PUT", "config/files/{}", Audience::Admin),
    ("POST", "config/entities/{}", Audience::Admin),
    ("PUT", "config/entities/{}/{}", Audience::Admin),
    ("DELETE", "config/entities/{}/{}", Audience::Admin),
    ("GET", "pools", Audience::Admin),
    ("GET", "pools/{}", Audience::Admin),
    ("GET", "reverse_proxy/upstreams", Audience::Admin),
    ("POST", "pools/{}/upstreams", Audience::Admin),
    ("PUT", "pools/{}/upstreams", Audience::Admin),
    ("POST", "pools/{}/upstreams/state", Audience::Admin),
    ("PUT", "pools/{}/upstreams/state", Audience::Admin),
    ("DELETE", "pools/{}/upstreams", Audience::Admin),
    ("POST", "reload", Audience::Admin),
    ("POST", "load", Audience::Admin),
    ("PATCH", "config/{}/{}", Audience::Admin),
    ("GET", "certificates", Audience::Admin),
    ("POST", "certificates/reload", Audience::Admin),
    ("POST", "upgrade", Audience::Admin),
    ("POST", "stop", Audience::Admin),
];

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
    /// TLS of the listener, from `[admin] tls`.
    pub(crate) tls: Option<AdminTls>,
    pub(crate) current: RwLock<Current>,
    pub(crate) control: mpsc::Sender<Control>,
    /// Held while a configuration file is saved and reloaded, so two saves
    /// do not write over each other.
    pub(crate) saving: tokio::sync::Mutex<()>,
    /// One permit for each password that may be hashed or compared at a
    /// time, see [`hashing_permits`].
    pub(crate) hashing: Arc<Semaphore>,
    /// The users file, from `[admin] users`.
    pub(crate) access: Store,
    /// Who is signed in.
    pub(crate) sessions: Sessions,
}

/// Who is at the other end of a connection to the admin API.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Peer {
    /// The connection comes from this machine: a unix socket, or a TCP peer
    /// for which [`connection_is_local`] holds.
    pub(crate) local: bool,
    /// The connection is encrypted.
    pub(crate) tls: bool,
    /// Address of a TCP peer.
    pub(crate) address: Option<IpAddr>,
}

impl Peer {
    /// Whether a session may start or be used on this connection: over TLS
    /// or from this machine. A password and the cookie it buys do not
    /// travel in clear text over the network.
    pub(crate) fn may_hold_session(self) -> bool {
        self.tls || self.local
    }
}

/// Passwords hashed or compared at a time for the requests of the admin
/// API. bcrypt takes a quarter of a second of a processor, and anybody can
/// ask for it by signing in.
const MAX_HASHES: usize = 4;

/// The permits of [`AdminState::hashing`].
pub(crate) fn hashing_permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_HASHES))
}

/// First byte of a TLS connection: the record type of a handshake.
const TLS_HANDSHAKE: u8 = 0x16;

/// Time a connection to a TLS listener has to send its first byte and
/// finish the handshake, so a silent peer does not hold a task and a
/// descriptor.
const OPENING_TIMEOUT: Duration = Duration::from_secs(10);

/// What a TLS listener does with a connection, from its first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// A handshake: served over TLS, wherever it comes from.
    Tls,
    /// Plain HTTP from this machine: served as it is.
    Plain,
    /// Plain HTTP from another machine: closed, so nothing travels in
    /// clear text over the network, a token least of all.
    Closed,
}

impl Opening {
    fn of(first: u8, local: bool) -> Self {
        match (first, local) {
            (TLS_HANDSHAKE, _) => Self::Tls,
            (_, true) => Self::Plain,
            (_, false) => Self::Closed,
        }
    }
}

/// A connection to a TLS listener that is ready for HTTP.
enum Opened {
    Tls(Box<TlsStream<TcpStream>>),
    Plain(TcpStream),
}

/// Reads how a connection to a TLS listener opens and finishes its
/// handshake, within `limit`. `None` when the connection is to be closed:
/// see [`Opening::Closed`], a failed handshake, or a peer that is too slow.
async fn open(
    stream: TcpStream,
    acceptor: &TlsAcceptor,
    local: bool,
    limit: Duration,
) -> Option<Opened> {
    let opening = async {
        let mut first = [0_u8; 1];
        if stream.peek(&mut first).await.ok()? == 0 {
            return None;
        }
        match Opening::of(first[0], local) {
            Opening::Tls => {
                let stream = acceptor.accept(stream).await.ok()?;
                Some(Opened::Tls(Box::new(stream)))
            }
            Opening::Plain => Some(Opened::Plain(stream)),
            Opening::Closed => None,
        }
    };
    tokio::time::timeout(limit, opening).await.ok().flatten()
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
                if let Ok((stream, address)) = listener.accept().await {
                    tokio::task::spawn(serve_tcp(stream, address.ip(), Arc::clone(&state)));
                }
            }
            #[cfg(unix)]
            Listener::Unix(listener, _) => {
                if let Ok((stream, _)) = listener.accept().await {
                    let peer = Peer {
                        local: true,
                        tls: false,
                        address: None,
                    };
                    tokio::task::spawn(serve_connection(stream, peer, Arc::clone(&state)));
                }
            }
        }
    }
}

/// True if a TCP connection from `peer`, accepted on the address `local` of
/// this machine, comes from this machine: the peer is a loopback address, or
/// the machine is talking to its own address. IPv4-mapped IPv6 addresses
/// count as the IPv4 address they hold.
fn connection_is_local(peer: IpAddr, local: IpAddr) -> bool {
    let peer = peer.to_canonical();
    // The unspecified address is nobody's: never equal to a local address.
    peer.is_loopback() || (peer == local.to_canonical() && !peer.is_unspecified())
}

/// Serves one TCP connection. On a TLS listener a connection that opens
/// with a handshake is served over TLS, one from this machine that opens
/// with anything else as plain HTTP, and any other is closed.
async fn serve_tcp(stream: TcpStream, address: IpAddr, state: Arc<AdminState>) {
    // A connection whose own address cannot be read is not local.
    let local = stream
        .local_addr()
        .is_ok_and(|local| connection_is_local(address, local.ip()));
    let peer = Peer {
        local,
        tls: false,
        address: Some(address),
    };
    let Some(acceptor) = state.tls.as_ref().map(|tls| tls.acceptor.clone()) else {
        return serve_connection(stream, peer, state).await;
    };
    match open(stream, &acceptor, peer.local, OPENING_TIMEOUT).await {
        Some(Opened::Tls(stream)) => {
            serve_connection(stream, Peer { tls: true, ..peer }, state).await;
        }
        Some(Opened::Plain(stream)) => serve_connection(stream, peer, state).await,
        None => {}
    }
}

async fn serve_connection<S>(stream: S, peer: Peer, state: Arc<AdminState>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |request| handle(request, Arc::clone(&state), peer));
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

async fn handle(
    request: Request<Incoming>,
    state: Arc<AdminState>,
    peer: Peer,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // The page calls the API with relative URLs, which only resolve to the
    // API routes from this exact path.
    let is_dashboard_page = request.uri().path() == "/dashboard";
    let method = request.method().clone();
    let path = request.uri().path().trim_matches('/').to_owned();

    let caller = caller(&request, &state, peer);
    if let Some(refused) = gate(&request, &state, peer, &caller, audience(&method, &path)) {
        return Ok(refused);
    }

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
        (&Method::GET, ["stats", "history"]) => {
            // `since=<unix seconds>`: only the samples taken after it.
            let since: Option<Result<u64, _>> = request.uri().query().and_then(|query| {
                query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("since="))
                    .map(str::parse)
            });
            match since.transpose() {
                // Polled every few seconds by the page: no indentation.
                Ok(since) => json_bytes(
                    StatusCode::OK,
                    serde_json::to_vec(&json!({
                        "interval_seconds": crate::history::INTERVAL.as_secs(),
                        "buckets": crate::metrics::BUCKETS,
                        "samples": crate::history::global().samples(since),
                    })),
                ),
                Err(_) => error(
                    StatusCode::BAD_REQUEST,
                    "since must be a unix time in seconds",
                ),
            }
        }
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
        (&Method::GET, ["auth", "session"]) => auth_api::session(&state, &caller, peer),
        (&Method::POST, ["auth", "login"]) => auth_api::login(&state, peer, request).await,
        (&Method::POST, ["auth", "logout"]) => auth_api::logout(&state, &request, peer),
        (&Method::POST, ["auth", "setup"]) => auth_api::setup(&state, &caller, peer, request).await,
        (&Method::PUT, ["auth", "password"]) => {
            auth_api::password(&state, &caller, peer, request).await
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

/// Who sends a request.
pub(crate) enum Caller {
    /// The holder of `[admin] token`: an administrator.
    Token,
    /// A local request to a daemon without a token: an administrator.
    Local,
    /// A signed-in user.
    User {
        name: String,
        /// What the roles of the user allow, as of this request.
        scope: Scope,
        /// Id of the session.
        session: String,
    },
    /// Anybody else.
    Nobody,
}

impl Caller {
    /// What the caller may do with hosts.
    pub(crate) fn scope(&self) -> Scope {
        match self {
            Self::Token | Self::Local => Scope::Admin,
            Self::User { scope, .. } => scope.clone(),
            Self::Nobody => Scope::Grants(Vec::new()),
        }
    }

    pub(crate) fn is_admin(&self) -> bool {
        match self {
            Self::Token | Self::Local => true,
            Self::User { scope, .. } => scope.is_admin(),
            Self::Nobody => false,
        }
    }

    /// The name that stands for the caller in the log.
    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Token => "token",
            Self::Local => "local",
            Self::User { name, .. } => name,
            Self::Nobody => "nobody",
        }
    }
}

/// Who sends `request`. The first that holds: the token, a session cookie,
/// a local request when no token is configured, nobody. A session comes
/// before the local rule, so a browser on the machine acts as its user; and
/// a session whose user is no longer in the users file is nobody's. A cookie
/// counts only where a session may be, see [`Peer::may_hold_session`].
fn caller<B>(request: &Request<B>, state: &AdminState, peer: Peer) -> Caller {
    if let Some(token) = &state.token
        && has_token(request, token)
    {
        return Caller::Token;
    }
    if peer.may_hold_session()
        && let Some(id) = session::cookie_of(request)
        && let Some(session) = state.sessions.get(id)
    {
        let access = state.access.read();
        return match roles(&access, &session) {
            Some(roles) => Caller::User {
                name: session.user,
                scope: access.scope(&roles),
                session: id.to_owned(),
            },
            None => Caller::Nobody,
        };
    }
    if state.token.is_none() && is_local_request(request, &state.listen, peer) {
        return Caller::Local;
    }
    Caller::Nobody
}

/// The roles of a session: its own when it carries them, else the ones its
/// user has in the users file now. `None` when that user is not there.
pub(crate) fn roles(access: &Access, session: &Session) -> Option<Vec<String>> {
    match &session.roles {
        Some(roles) => Some(roles.clone()),
        None => access
            .user
            .iter()
            .find(|user| user.name == session.user)
            .map(|user| user.roles.clone()),
    }
}

/// Who the route of `method` and `path` (without its outer slashes, not
/// decoded) answers. A route that [`ROUTES`] does not list is for
/// administrators, and so is a listed one spelled another way.
fn audience(method: &Method, path: &str) -> Audience {
    let is_route = |pattern: &str| {
        let mut segments = path.split('/');
        pattern.split('/').all(|expected| {
            segments
                .next()
                .is_some_and(|s| expected == "{}" || expected == s)
        }) && segments.next().is_none()
    };
    ROUTES
        .iter()
        .find(|(route_method, pattern, _)| method.as_str() == *route_method && is_route(pattern))
        .map_or(Audience::Admin, |(_, _, audience)| *audience)
}

/// The check every request goes through before its route: the answer that
/// refuses `caller` on a route for `audience`, or `None` to let it through.
fn gate<B>(
    request: &Request<B>,
    state: &AdminState,
    peer: Peer,
    caller: &Caller,
    audience: Audience,
) -> Option<Response<Full<Bytes>>> {
    let reads = matches!(*request.method(), Method::GET | Method::HEAD);
    // A browser sends the cookie by itself; `Origin` tells whose page
    // asked. `SameSite=Strict` already keeps other sites out.
    if matches!(caller, Caller::User { .. }) && !reads && !origin_is_host(request) {
        return Some(error(StatusCode::FORBIDDEN, COOKIE_NEEDS_ORIGIN));
    }
    // A sign-in sent by the page of another site would sign the browser in
    // as the account of that site's choice. `curl` sends no `Origin`.
    let foreign = request.headers().contains_key(header::ORIGIN) && !origin_is_host(request);
    if audience == Audience::Anyone && !reads && foreign {
        return Some(error(
            StatusCode::FORBIDDEN,
            "the Origin of this request is not its Host",
        ));
    }
    match caller {
        Caller::Token | Caller::Local => None,
        Caller::User { scope, .. } => {
            if audience == Audience::Admin && !scope.is_admin() {
                return Some(error(StatusCode::FORBIDDEN, "this needs the admin role"));
            }
            None
        }
        Caller::Nobody => {
            // What a page of another site sends from a browser of this
            // machine gets nothing, not even the dashboard.
            if state.token.is_none()
                && peer.local
                && !is_local_request(request, &state.listen, peer)
            {
                return Some(error(StatusCode::FORBIDDEN, NOT_LOCAL));
            }
            if audience == Audience::Anyone {
                return None;
            }
            if state.token.is_none() {
                return Some(error(
                    StatusCode::UNAUTHORIZED,
                    "sign in to use the admin API",
                ));
            }
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                "admin API requires Authorization: Bearer <token>",
            );
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
            Some(response)
        }
    }
}

/// Answer to a request that changes something with a session cookie and
/// without the `Origin` of a page of this API.
pub(crate) const COOKIE_NEEDS_ORIGIN: &str =
    "a request with a session cookie needs an Origin equal to its Host";

/// True if the request has an `Origin` and it is the `Host` the request was
/// sent to, port included: the page that sent it came from this API.
pub(crate) fn origin_is_host<B>(request: &Request<B>) -> bool {
    let text = |name: header::HeaderName| {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
    };
    let authority = text(header::ORIGIN).and_then(|origin| origin.split_once("://"));
    authority
        .zip(text(header::HOST))
        .is_some_and(|((_, authority), host)| authority.eq_ignore_ascii_case(host))
}

/// Whether the setup code may create the first user on this connection:
/// only over TLS, since the code and the new password travel with it, and
/// only without a token, whose holder is the one to create the user.
pub(crate) fn code_opens_setup(peer: Peer, has_token: bool) -> bool {
    peer.tls && !has_token
}

/// Whether the daemon writes the setup code to its log when it starts: on
/// a listener where the code can be used, see [`code_opens_setup`], and
/// while there is no user.
pub(crate) fn announces_setup_code(has_tls: bool, has_token: bool, has_user: bool) -> bool {
    has_tls && !has_token && !has_user
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

/// Reads every certificate file again, changed or not, the one of the admin
/// listener included. Files that fail
/// keep their previous certificate, and make the answer an error.
fn reload_certificates(state: &AdminState) -> Response<Full<Bytes>> {
    let mut resolvers: Vec<_> = state
        .current
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .servers
        .iter()
        .filter_map(|server| server.cert_resolver.clone())
        .collect();
    resolvers.extend(state.tls.as_ref().map(|tls| Arc::clone(&tls.resolver)));
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

/// True if a request without a token comes from this machine and is not one
/// a web page of another origin can send. The connection must be local, see
/// [`Peer::local`]: headers say whatever the sender likes. `Host` must name a loopback host, or the address of a TCP
/// listener: a page that reaches the API through DNS rebinding sends its own
/// name. `Origin`, which browsers add to every request that can change
/// something, must be that same `Host`: the port counts, so the page of
/// another web server on this machine is refused. The comparison is with
/// `Host` and not with the listen address because a tunnel to the API, or to
/// its unix socket, can be on any local port. A request without `Host` and
/// `Origin` passes: browsers always send `Host`.
fn is_local_request<B>(request: &Request<B>, listen: &AdminListen, peer: Peer) -> bool {
    if !peer.local {
        return false;
    }
    // `None` for a header that is not text: refused.
    let text = |name: header::HeaderName| -> Option<Option<&str>> {
        match request.headers().get(name) {
            Some(value) => value.to_str().ok().map(Some),
            None => Some(None),
        }
    };
    let (Some(host), Some(origin)) = (text(header::HOST), text(header::ORIGIN)) else {
        return false;
    };

    let is_loopback = |authority: &str| {
        let name = match authority.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or_default(),
            None => authority.split(':').next().unwrap_or_default(),
        };
        name.eq_ignore_ascii_case("localhost")
            || matches!(name, "127.0.0.1" | "::1")
            || matches!(listen, AdminListen::Tcp(listen) if name == listen.ip().to_string())
    };
    let is_same_origin = |origin: &str| {
        // `null`, the origin of sandboxed pages and files, has no scheme.
        origin
            .split_once("://")
            .zip(host)
            .is_some_and(|((_, authority), host)| authority.eq_ignore_ascii_case(host))
    };

    host.is_none_or(is_loopback) && origin.is_none_or(is_same_origin)
}

/// Answer to a request from this machine that fails [`is_local_request`].
const NOT_LOCAL: &str = "admin API only accepts a loopback Host and its own Origin";

pub(crate) async fn read_json<T: for<'de> Deserialize<'de>>(
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
    json_bytes(status, serde_json::to_vec_pretty(value))
}

fn json_bytes(status: StatusCode, body: serde_json::Result<Vec<u8>>) -> Response<Full<Bytes>> {
    let body = body.unwrap_or_else(|err| {
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
pub(crate) mod tests {
    use std::net::Ipv4Addr;

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
                // An absolute API path breaks the page behind a path prefix.
                for banned in [
                    "innerHTML",
                    "{@html",
                    "style=",
                    "api(\"/",
                    "api(`/",
                    "api('/",
                ] {
                    assert!(!text.contains(banned), "{} holds {banned}", path.display());
                }
                // Every call goes through `api`, which keeps paths relative
                // and sends the token.
                let name = path.to_string_lossy();
                if !name.ends_with("lib/api.ts") && !name.contains(".test.") {
                    assert!(!text.contains("fetch("), "{} calls fetch", path.display());
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

    /// A connection from this machine.
    pub(crate) const LOCAL: Peer = Peer {
        local: true,
        tls: false,
        address: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
    };

    #[test]
    fn a_loopback_or_own_address_peer_is_local() {
        let local = |peer: &str, local: &str| {
            connection_is_local(peer.parse().unwrap(), local.parse().unwrap())
        };
        assert!(local("127.0.0.1", "127.0.0.1"));
        assert!(local("127.0.0.1", "192.0.2.7"));
        assert!(local("::1", "::1"));
        assert!(local("192.0.2.7", "192.0.2.7"));
        assert!(local("::ffff:127.0.0.1", "::"));
        // A dual stack socket reports IPv4 peers as mapped addresses.
        assert!(local("::ffff:192.0.2.7", "192.0.2.7"));
        assert!(local("192.0.2.7", "::ffff:192.0.2.7"));
    }

    #[test]
    fn another_address_is_not_local() {
        let local = |peer: &str, local: &str| {
            connection_is_local(peer.parse().unwrap(), local.parse().unwrap())
        };
        assert!(!local("192.0.2.8", "192.0.2.7"));
        assert!(!local("::ffff:192.0.2.8", "192.0.2.7"));
        assert!(!local("2001:db8::8", "2001:db8::7"));
        // The peer of a listener on every address is not that address.
        assert!(!local("192.0.2.8", "0.0.0.0"));
        assert!(!local("0.0.0.0", "192.0.2.7"));
    }

    #[test]
    fn a_forged_loopback_host_from_a_remote_peer_is_not_a_local_request() {
        let remote = Peer {
            local: false,
            tls: true,
            address: Some("192.0.2.8".parse().unwrap()),
        };
        let wide = AdminListen::Tcp("0.0.0.0:2019".parse().unwrap());
        for host in [
            "localhost:2019",
            "127.0.0.1:2019",
            "[::1]:2019",
            "0.0.0.0:2019",
        ] {
            assert!(
                is_local_request(&request(host, None), &wide, LOCAL),
                "{host}"
            );
            assert!(
                !is_local_request(&request(host, None), &wide, remote),
                "{host}"
            );
        }
        let same_origin = request("localhost:2019", Some("http://localhost:2019"));
        assert!(!is_local_request(&same_origin, &wide, remote));
        assert!(
            !is_local_request(&Request::new(()), &wide, remote),
            "no Host"
        );
    }

    /// A TLS connection from another machine.
    pub(crate) const REMOTE: Peer = Peer {
        local: false,
        tls: true,
        address: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8))),
    };

    /// The state of a daemon that listens on every address, with the users
    /// file `users`.
    pub(crate) fn state(token: Option<&str>, users: PathBuf) -> AdminState {
        let config = Config::default();
        let registry = Registry::build(&config).unwrap();
        let (control, _) = mpsc::channel(1);
        AdminState {
            started: Instant::now(),
            config_path: None,
            listen: AdminListen::Tcp("0.0.0.0:2019".parse().unwrap()),
            token: token.map(str::to_owned),
            tls: None,
            current: RwLock::new(Current {
                config: Arc::new(config),
                registry: Arc::new(registry),
                servers: Vec::new(),
            }),
            control,
            saving: tokio::sync::Mutex::new(()),
            hashing: hashing_permits(),
            access: Store::open(users),
            sessions: Sessions::new(),
        }
    }

    #[tokio::test]
    async fn a_remote_peer_with_a_loopback_host_is_nobody() {
        let open = state(None, PathBuf::new());
        for host in ["localhost:2019", "127.0.0.1:2019", "0.0.0.0:2019"] {
            let forged = request(host, None);
            assert!(matches!(caller(&forged, &open, LOCAL), Caller::Local));
            let nobody = caller(&forged, &open, REMOTE);
            assert!(matches!(nobody, Caller::Nobody), "{host}");
            assert!(!nobody.is_admin());
            assert_eq!(nobody.scope(), Scope::Grants(Vec::new()));
            // Refused on a route for administrators, served the sign-in.
            let refused = gate(&forged, &open, REMOTE, &nobody, Audience::Admin).unwrap();
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
            assert!(!refused.headers().contains_key(header::WWW_AUTHENTICATE));
            let refused = gate(&forged, &open, REMOTE, &nobody, Audience::Caller).unwrap();
            assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
            assert!(gate(&forged, &open, REMOTE, &nobody, Audience::Anyone).is_none());
        }
        let same_origin = request("localhost:2019", Some("http://localhost:2019"));
        assert!(matches!(
            caller(&same_origin, &open, REMOTE),
            Caller::Nobody
        ));
    }

    #[tokio::test]
    async fn the_token_comes_first_and_a_local_request_last() {
        let directory = tempfile::tempdir().unwrap();
        let users = directory.path().join("users.toml");
        let with = |authorization: Option<&str>, cookie: Option<&str>| {
            let mut builder = Request::builder().header(header::HOST, "localhost:2019");
            if let Some(authorization) = authorization {
                builder = builder.header(header::AUTHORIZATION, authorization);
            }
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, format!("rxh_session={cookie}"));
            }
            builder.body(()).unwrap()
        };

        let closed = state(Some("0123456789abcdef"), users.clone());
        closed
            .access
            .change(|access| access.add_user("luis", "password-luis", &[]))
            .unwrap();
        let id = closed.sessions.create("luis", None);
        let bearer = Some("Bearer 0123456789abcdef");
        assert!(matches!(
            caller(&with(bearer, Some(&id)), &closed, REMOTE),
            Caller::Token
        ));
        // With a token configured, being local is nothing.
        assert!(matches!(
            caller(&with(None, None), &closed, LOCAL),
            Caller::Nobody
        ));
        assert!(matches!(
            caller(&with(Some("Bearer wrong"), None), &closed, LOCAL),
            Caller::Nobody
        ));
        let user = caller(&with(Some("Bearer wrong"), Some(&id)), &closed, REMOTE);
        assert!(matches!(&user, Caller::User { name, scope, session }
                if name == "luis" && !scope.is_admin() && *session == id));
        assert_eq!(user.label(), "luis");
        assert!(!user.is_admin());

        // Without a token a session still comes before the local rule.
        let open = state(None, users);
        let id = open.sessions.create("luis", None);
        assert!(matches!(
            caller(&with(None, Some(&id)), &open, LOCAL),
            Caller::User { .. }
        ));
        assert!(matches!(
            caller(&with(None, Some("no-such-session")), &open, LOCAL),
            Caller::Local
        ));
        // A session of a user that is not in the file is nobody's, also on
        // the machine.
        let gone = open.sessions.create("eva", None);
        assert!(matches!(
            caller(&with(None, Some(&gone)), &open, LOCAL),
            Caller::Nobody
        ));
        // Roles a session carries need no user in the file.
        let carried = open
            .sessions
            .create("eva", Some(vec![String::from("admin")]));
        assert!(caller(&with(None, Some(&carried)), &open, REMOTE).is_admin());
    }

    #[tokio::test]
    async fn a_sign_in_from_the_page_of_another_site_is_refused() {
        let closed = state(Some("0123456789abcdef"), PathBuf::new());
        let post = |origin: Option<&str>| {
            let mut sent = request("rxh.example:2019", origin);
            *sent.method_mut() = Method::POST;
            sent
        };
        let answer = |sent: &Request<()>, caller: &Caller| {
            gate(sent, &closed, REMOTE, caller, Audience::Anyone).map(|refused| refused.status())
        };
        for caller in [Caller::Nobody, Caller::Token] {
            // `curl` sends no `Origin`, the page sends its own.
            assert_eq!(answer(&post(None), &caller), None);
            assert_eq!(
                answer(&post(Some("https://rxh.example:2019")), &caller),
                None
            );
            for origin in ["https://evil.example", "null", "https://rxh.example"] {
                assert_eq!(
                    answer(&post(Some(origin)), &caller),
                    Some(StatusCode::FORBIDDEN),
                    "{origin}"
                );
            }
        }
        // Reading the page and the session from anywhere changes nothing.
        let read = request("rxh.example:2019", Some("https://evil.example"));
        assert_eq!(answer(&read, &Caller::Nobody), None);
    }

    #[tokio::test]
    async fn a_cookie_counts_only_over_tls_or_from_this_machine() {
        let directory = tempfile::tempdir().unwrap();
        let closed = state(
            Some("0123456789abcdef"),
            directory.path().join("users.toml"),
        );
        closed
            .access
            .change(|access| access.add_user("ana", "password-ana", &[String::from("admin")]))
            .unwrap();
        let id = closed.sessions.create("ana", None);
        let sent = Request::builder()
            .header(header::HOST, "rxh.example:2019")
            .header(header::COOKIE, format!("rxh_session={id}"))
            .body(())
            .unwrap();
        let plain = Peer {
            tls: false,
            ..REMOTE
        };
        assert!(!plain.may_hold_session());
        assert!(matches!(caller(&sent, &closed, plain), Caller::Nobody));
        assert!(matches!(
            caller(&sent, &closed, REMOTE),
            Caller::User { .. }
        ));
        assert!(matches!(caller(&sent, &closed, LOCAL), Caller::User { .. }));
    }

    #[test]
    fn the_setup_code_is_logged_only_where_it_works_and_while_no_user_exists() {
        // TLS, a token, a user: is the code written to the log?
        for (has_tls, has_token, has_user, logged) in [
            (true, false, false, true),
            (true, false, true, false),
            (true, true, false, false),
            (true, true, true, false),
            (false, false, false, false),
            (false, false, true, false),
            (false, true, false, false),
            (false, true, true, false),
        ] {
            assert_eq!(
                announces_setup_code(has_tls, has_token, has_user),
                logged,
                "tls {has_tls}, token {has_token}, user {has_user}"
            );
        }
    }

    #[test]
    fn setup_with_code_needs_tls_and_no_token() {
        let plain = Peer {
            tls: false,
            ..REMOTE
        };
        assert!(code_opens_setup(REMOTE, /* has_token= */ false));
        assert!(!code_opens_setup(REMOTE, /* has_token= */ true));
        assert!(!code_opens_setup(plain, /* has_token= */ false));
        // Plain HTTP on the machine: a local request needs no code, and one
        // that is not local (a page of another site) must not use it.
        assert!(!code_opens_setup(LOCAL, /* has_token= */ false));
        assert!(!code_opens_setup(LOCAL, /* has_token= */ true));
    }

    #[test]
    fn a_route_outside_the_table_is_for_administrators() {
        let of = |method: Method, path: &str| audience(&method, path);
        assert_eq!(of(Method::GET, "dashboard"), Audience::Anyone);
        assert_eq!(of(Method::GET, "auth/session"), Audience::Anyone);
        assert_eq!(of(Method::POST, "auth/login"), Audience::Anyone);
        assert_eq!(of(Method::POST, "auth/logout"), Audience::Anyone);
        assert_eq!(of(Method::PUT, "auth/password"), Audience::Caller);
        assert_eq!(of(Method::GET, "pools/api"), Audience::Admin);
        // Another method, a longer or shorter path, another spelling.
        for (method, path) in [
            (Method::POST, "dashboard"),
            (Method::GET, "auth/login"),
            (Method::DELETE, "auth/session"),
            (Method::GET, "auth/session/x"),
            (Method::GET, "auth"),
            (Method::GET, "dashboard//app.css"),
            (Method::GET, "auth/%73ession"),
            (Method::GET, "AUTH/SESSION"),
            (Method::GET, "nope"),
        ] {
            assert_eq!(of(method.clone(), path), Audience::Admin, "{method} {path}");
        }
    }

    #[test]
    fn every_route_of_the_router_is_in_the_table() {
        // The arms of the router, read from this file: `(&Method::GET, [] |
        // ["pools", name]) =>` is `GET ""` and `GET "pools/{}"`.
        let source = include_str!("admin.rs");
        let router = source.split("#[cfg(test)]").next().unwrap();
        let mut routed: Vec<(String, String)> = Vec::new();
        for line in router.lines() {
            let Some(arm) = line.trim().strip_prefix("(&Method::") else {
                continue;
            };
            let (methods, patterns) = arm.split_once(", ").unwrap();
            let patterns = patterns.split(") ").next().unwrap();
            for method in methods.split(" | ") {
                let method = method.trim_start_matches("&Method::");
                for pattern in patterns.split(" | ") {
                    let inner = pattern.trim_matches(['[', ']']);
                    let segments: Vec<&str> = inner
                        .split(", ")
                        .map(|segment| match segment.strip_prefix('"') {
                            Some(literal) => literal.trim_end_matches('"'),
                            None if segment.is_empty() => "",
                            None => "{}",
                        })
                        .collect();
                    routed.push((method.to_owned(), segments.join("/")));
                }
            }
        }
        assert!(routed.len() > 30, "the scan must find the arms: {routed:?}");
        for (method, path) in &routed {
            assert!(
                ROUTES
                    .iter()
                    .any(|(listed, pattern, _)| listed == method && pattern == path),
                "{method} /{path} is routed and missing from ROUTES"
            );
        }
        // And the table lists nothing the router does not have, which would
        // also mean that the scan missed an arm.
        for (index, (method, path, _)) in ROUTES.iter().enumerate() {
            assert!(
                routed
                    .iter()
                    .any(|(routed_method, routed_path)| routed_method == method
                        && routed_path == path),
                "{method} /{path} is in ROUTES and not routed"
            );
            assert!(
                !ROUTES[..index]
                    .iter()
                    .any(|(other, pattern, _)| other == method && pattern == path),
                "{method} /{path} is in ROUTES twice"
            );
        }
    }

    #[test]
    fn a_cookie_request_that_changes_something_needs_its_own_origin() {
        let own = |host: &str, origin: &str| origin_is_host(&request(host, Some(origin)));
        assert!(own("rxh.example:2019", "https://rxh.example:2019"));
        assert!(own("localhost:2019", "http://LOCALHOST:2019"));
        assert!(!own("rxh.example:2019", "https://evil.example"));
        assert!(!own("rxh.example:2019", "https://rxh.example"));
        assert!(!own("rxh.example:2019", "https://rxh.example:2019/"));
        assert!(!own("rxh.example:2019", "null"));
        assert!(!own("rxh.example:2019", "rxh.example:2019"));
        assert!(!origin_is_host(&request("rxh.example:2019", None)));
        assert!(!origin_is_host(&Request::new(())));
    }

    #[test]
    fn plain_http_from_another_machine_is_closed_on_a_tls_listener() {
        for local in [true, false] {
            assert_eq!(Opening::of(0x16, local), Opening::Tls);
        }
        // `G` of `GET`, and a byte that starts neither protocol.
        for first in [b'G', 0x00, 0x15, 0x17, 0x80] {
            assert_eq!(Opening::of(first, /* local= */ true), Opening::Plain);
            assert_eq!(Opening::of(first, /* local= */ false), Opening::Closed);
        }
    }

    /// The acceptor of a new certificate, and an accepted connection to it
    /// with its client end.
    async fn tls_listener() -> (TlsAcceptor, TcpStream, TcpStream) {
        let generated =
            rcgen::generate_simple_self_signed(vec![String::from("localhost")]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = (dir.path().join("admin.pem"), dir.path().join("admin.key"));
        std::fs::write(&cert, generated.cert.pem()).unwrap();
        std::fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
        let tls = AdminTls::from_files(cert.to_str().unwrap(), key.to_str().unwrap()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        (tls.acceptor, accepted, client)
    }

    #[tokio::test]
    async fn a_silent_connection_to_a_tls_listener_is_closed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let limit = Duration::from_millis(100);
        // Nothing sent, then a handshake that stops after its first byte.
        for sent in [&b""[..], &[0x16][..]] {
            let (acceptor, accepted, mut client) = tls_listener().await;
            client.write_all(sent).await.unwrap();
            let started = Instant::now();
            let opened = open(accepted, &acceptor, /* local= */ true, limit).await;
            assert!(opened.is_none(), "{sent:?}");
            assert!(
                started.elapsed() >= limit,
                "{sent:?}: closed before the limit"
            );
            let mut rest = Vec::new();
            let read = tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut rest))
                .await
                .expect("the daemon closes the connection");
            assert!(matches!(read, Ok(0) | Err(_)), "{sent:?}: {read:?}");
        }

        // A client that speaks in time is served.
        let (acceptor, accepted, mut client) = tls_listener().await;
        client
            .write_all(b"GET /status HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let opened = open(accepted, &acceptor, /* local= */ true, limit).await;
        assert!(matches!(opened, Some(Opened::Plain(_))));
        let (acceptor, accepted, mut client) = tls_listener().await;
        client
            .write_all(b"GET /status HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let opened = open(accepted, &acceptor, /* local= */ false, limit).await;
        assert!(opened.is_none(), "plain HTTP from another machine");
    }

    #[test]
    fn only_loopback_hosts_are_allowed() {
        let tcp = AdminListen::Tcp("127.0.0.1:2019".parse().unwrap());
        let unix = AdminListen::Unix(PathBuf::from("/run/rxh/admin.sock"));
        for listen in [&tcp, &unix] {
            let local = |host: &str, origin: Option<&str>| {
                is_local_request(&request(host, origin), listen, LOCAL)
            };
            for host in [
                "localhost",
                "localhost:2019",
                "LocalHost:2019",
                "127.0.0.1:2019",
                "[::1]:2019",
            ] {
                assert!(local(host, None), "{host}");
            }
            for host in [
                "evil.example",
                "evil.example:2019",
                "127.0.0.1.evil.example",
                "localhost.evil.example:2019",
                "",
            ] {
                assert!(!local(host, None), "{host}");
            }
            assert!(
                is_local_request(&Request::new(()), listen, LOCAL),
                "no Host"
            );
        }

        // The address of a TCP listener is a name of the API, on TCP only.
        let wide = AdminListen::Tcp("10.0.0.5:2019".parse().unwrap());
        assert!(is_local_request(
            &request("10.0.0.5:2019", None),
            &wide,
            LOCAL
        ));
        assert!(!is_local_request(
            &request("10.0.0.5:2019", None),
            &tcp,
            LOCAL
        ));
        assert!(!is_local_request(
            &request("10.0.0.5:2019", None),
            &unix,
            LOCAL
        ));
    }

    #[test]
    fn an_origin_must_be_the_host_of_the_request() {
        let tcp = AdminListen::Tcp("127.0.0.1:2019".parse().unwrap());
        let unix = AdminListen::Unix(PathBuf::from("/run/rxh/admin.sock"));
        for listen in [&tcp, &unix] {
            let local = |host: &str, origin: &str| {
                is_local_request(&request(host, Some(origin)), listen, LOCAL)
            };
            assert!(local("localhost:2019", "http://localhost:2019"));
            assert!(local("127.0.0.1:2019", "http://127.0.0.1:2019"));
            assert!(local("[::1]:2019", "http://[::1]:2019"));
            assert!(local("localhost", "http://localhost"));
            assert!(local("localhost:2019", "HTTP://LOCALHOST:2019"));
            // A tunnel on a port that is not the one the daemon listens on.
            assert!(local("localhost:8080", "http://localhost:8080"));

            // Another site, and another web server of this machine.
            assert!(!local("localhost:2019", "https://evil.example"));
            assert!(!local("localhost:2019", "http://localhost:3000"));
            assert!(!local("localhost:2019", "http://localhost"));
            assert!(!local("localhost", "http://localhost:2019"));
            assert!(!local("localhost:2019", "http://127.0.0.1:2019"));
            // The same origin, but not of this machine.
            assert!(!local("evil.example", "http://evil.example"));
            // Not an origin.
            assert!(!local("localhost:2019", "null"));
            assert!(!local("localhost:2019", ""));
            assert!(!local("localhost:2019", "localhost:2019"));
            assert!(!local("localhost:2019", "http://localhost:2019/"));
            assert!(!local("localhost:2019", "http://user@localhost:2019"));

            // Nothing to compare the origin with.
            let mut no_host = Request::new(());
            no_host.headers_mut().insert(
                header::ORIGIN,
                header::HeaderValue::from_static("http://localhost:2019"),
            );
            assert!(!is_local_request(&no_host, listen, LOCAL));
        }
    }
}
