//! One listening socket and the connections it accepts.
//!
//! A `[[server]]` with several `listen` addresses becomes several [`Server`]
//! values, and HTTP servers listing the same address share one [`Server`]
//! (see [`crate::Master`]). Each one accepts connections, limits them
//! to `max_connections`, and on shutdown stops accepting, lets open
//! connections finish (up to `grace_period`) and reports progress through
//! [`State`].

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hyper::header::HeaderValue;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use rxh_core::config::{Action, Config, ConfigError, Protocol, ProxyProtocol};
use rxh_core::net::{self, IpNet};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::acme;
use crate::error::Error;
use crate::http::headers;
use crate::http::service::{Activity, Route, Service, Site};
use crate::l4match::{self, Detected, L4Matcher, Rewind};
use crate::log::{ListenerLog, Logger};
use crate::ocsp;
use crate::proxy_protocol;
use crate::registry::Registry;
use crate::systemd;
use crate::tcp;
use crate::timeout::{Stamp, Watched};
use crate::tls::{self, CertResolver};
use crate::udp::{self, UdpSettings};

/// Deadline for the TLS handshake when `read_header_timeout` is unset.
const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for the first request head of an HTTP connection when
/// `read_header_timeout` is unset. Hyper applies the same default to later
/// HTTP/1 request heads.
const DEFAULT_READ_HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// Current state of a [`Server`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum State {
    /// Initialized but not accepting connections yet.
    Starting,

    /// Accepting connections.
    Listening,

    /// `max_connections` reached, new connections wait in the backlog.
    MaxConnectionsReached(usize),

    /// Graceful shutdown in progress.
    ShuttingDown(ShutdownState),
}

/// Progress of a graceful shutdown.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ShutdownState {
    /// No longer accepting, waiting for this many open connections.
    PendingConnections(usize),

    /// Shutdown complete.
    Done,
}

/// What accepted TCP connections are handed to.
#[derive(Clone)]
enum Handler {
    Http {
        routes: Arc<[Route]>,
        http2: bool,
    },
    Tcp {
        routes: Arc<[tcp::Route]>,
        /// True if picking a route needs the client's first bytes.
        sniff: bool,
        proxy_protocol: Option<ProxyProtocol>,
        limits: tcp::Limits,
    },
}

enum Socket {
    Tcp(TcpListener),
    Udp(Arc<UdpSocket>, UdpSettings),
}

/// Shared by every connection task of one server.
struct ConnectionContext {
    handler: Handler,
    tls: Option<TlsAcceptor>,
    log: ListenerLog,
    read_header_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
    /// Read a PROXY header first, from peers in `trusted_proxies` (any peer
    /// if empty).
    proxy_protocol_in: bool,
    trusted_proxies: Vec<IpNet>,
    /// `ip_allow` and `ip_deny` of a `tcp` server, checked on accept. HTTP
    /// servers check theirs per request, after `X-Forwarded-For`.
    ip_allow: Vec<IpNet>,
    ip_deny: Vec<IpNet>,
    /// `max_connections_per_ip` and the open connections of each client.
    per_ip: Option<(usize, Mutex<HashMap<IpAddr, usize>>)>,
    match_timeout: Duration,
}

/// One connection counted against `max_connections_per_ip`, until dropped.
struct PerIpSlot<'a> {
    counts: &'a Mutex<HashMap<IpAddr, usize>>,
    ip: IpAddr,
}

impl<'a> PerIpSlot<'a> {
    /// Counts one more connection of `ip`, or `None` if it already has `max`.
    fn acquire(max: usize, counts: &'a Mutex<HashMap<IpAddr, usize>>, ip: IpAddr) -> Option<Self> {
        let ip = ip.to_canonical();
        let mut map = counts
            .lock()
            .expect("nothing panics while holding the connection counts");
        let count = map.entry(ip).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Self { counts, ip })
    }
}

impl Drop for PerIpSlot<'_> {
    fn drop(&mut self) {
        // Dropped during a panic of the connection task too, where a second
        // panic would abort: a poisoned lock is left alone.
        let Ok(mut map) = self.counts.lock() else {
            return;
        };
        if let Some(count) = map.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&self.ip);
            }
        }
    }
}

/// What the listeners of the other runtimes share with the [`Server`] they
/// replicate: its address, its routes with their state, and its limits.
#[derive(Clone)]
pub struct Replica {
    address: SocketAddr,
    protocol: Protocol,
    name: Option<String>,
    log_name: Arc<str>,
    max_connections: usize,
    grace_period: Option<Duration>,
    context: Arc<ConnectionContext>,
    permits: Arc<Semaphore>,
    closing: watch::Sender<bool>,
}

/// One listener. See the module documentation.
pub struct Server {
    state: watch::Sender<State>,
    socket: Socket,
    address: SocketAddr,
    protocol: Protocol,
    name: Option<String>,
    log_name: Arc<str>,
    max_connections: usize,
    /// One permit per open connection, shared with the replicas.
    permits: Arc<Semaphore>,
    grace_period: Option<Duration>,
    context: Option<Arc<ConnectionContext>>,
    cert_resolver: Option<Arc<CertResolver>>,
    /// Keeps OCSP responses stapled to the certificates of `cert_resolver`.
    ocsp_stapling: bool,
    /// False for replicas, which do not repeat the messages of the first
    /// listener.
    announce: bool,
    /// Raised on shutdown, for `stream_close_delay`.
    closing: watch::Sender<bool>,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Server {
    /// Binds `requested` for the servers `config.servers[index]` of every
    /// index in `members` without accepting connections yet. Only HTTP
    /// servers can share a listener: their routes are tried in the order of
    /// `members`, and the first member decides the protocol, `http2` and
    /// `max_connections`. Port 0 picks a free port, see
    /// [`Server::socket_address`].
    ///
    /// # Panics
    ///
    /// Panics if `members` is empty or has an index out of bounds.
    pub fn init(
        config: &Config,
        registry: &Registry,
        requested: SocketAddr,
        members: &[usize],
    ) -> Result<Self, Error> {
        let server_index = members[0];
        let server = &config.servers[server_index];
        let servers: Vec<_> = members
            .iter()
            .map(|&index| &config.servers[index])
            .collect();

        let (socket, address) = match server.protocol {
            Protocol::Udp => {
                let socket = match systemd::udp_socket(requested)? {
                    Some(socket) => socket,
                    None => std::net::UdpSocket::bind(requested)?,
                };
                socket.set_nonblocking(true)?;
                let socket = UdpSocket::from_std(socket)?;
                let address = socket.local_addr()?;
                (SocketKind::Udp(Arc::new(socket)), address)
            }
            Protocol::Http | Protocol::Tcp => {
                let listener = bind_tcp(requested)?;
                let address = listener.local_addr()?;
                (SocketKind::Tcp(listener), address)
            }
        };

        let site_log_name = |name: Option<&str>| match name {
            Some(name) => format!("{address} ({name})"),
            None => address.to_string(),
        };
        let names: Vec<&str> = servers
            .iter()
            .filter_map(|server| server.name.as_deref())
            .collect();
        let name = (!names.is_empty()).then(|| names.join(", "));
        let log_name: Arc<str> = site_log_name(name.as_deref()).into();
        let label: Arc<str> = name.clone().unwrap_or_else(|| address.to_string()).into();
        let logger = Arc::new(Logger::new(&config.log)?);

        let mut cert_resolver = None;
        let closing = watch::Sender::new(false);
        let (socket, context) = match socket {
            SocketKind::Udp(socket) => {
                let settings = UdpSettings {
                    group: {
                        let Action::Forward(forward) = &server.patterns[0].action else {
                            unreachable!("validation guarantees udp servers forward")
                        };
                        // Validation rejects splits on UDP servers: one group.
                        Arc::clone(registry.resolve(forward, server_index, server, 0)?.pick())
                    },
                    idle_timeout: server.udp_idle_timeout,
                    max_sessions: server.max_connections,
                    ip_allow: server.ip_allow.clone(),
                    ip_deny: server.ip_deny.clone(),
                    log_name: Arc::clone(&log_name),
                    server: Arc::clone(&label),
                    logger: Arc::clone(&logger),
                };
                (Socket::Udp(socket, settings), None)
            }
            SocketKind::Tcp(listener) => {
                let handler = match server.protocol {
                    Protocol::Tcp => {
                        let mut routes = Vec::new();
                        for (index, pattern) in server.patterns.iter().enumerate() {
                            let Action::Forward(forward) = &pattern.action else {
                                unreachable!("validation guarantees tcp servers forward")
                            };
                            let matcher = L4Matcher::new(pattern).map_err(|err| {
                                Error::Config(ConfigError::Invalid(format!(
                                    "match[{index}]: {err}"
                                )))
                            })?;
                            let split = registry.resolve(forward, server_index, server, index)?;
                            routes.push(tcp::Route { matcher, split });
                        }
                        // Entries after the first one without matchers never match.
                        let terminated = server.tls.is_some();
                        let sniff = routes
                            .iter()
                            .take_while(|route| !route.matcher.is_default())
                            .any(|route| route.matcher.needs_bytes(terminated));
                        Handler::Tcp {
                            routes: routes.into(),
                            sniff,
                            proxy_protocol: server.proxy_protocol,
                            limits: tcp::Limits {
                                idle: server.idle_timeout,
                                stream: server.stream_timeout,
                            },
                        }
                    }
                    _ => {
                        let mut routes = Vec::new();
                        for (&index, member) in members.iter().zip(&servers) {
                            Site::build_routes(
                                index,
                                member,
                                registry,
                                site_log_name(member.name.as_deref()),
                                address,
                                &closing.subscribe(),
                                &mut routes,
                            )?;
                        }
                        Handler::Http {
                            routes: routes.into(),
                            http2: server.http2,
                        }
                    }
                };
                let tls = tls::acceptor(&servers)?.map(|(acceptor, resolver)| {
                    cert_resolver = Some(resolver);
                    acceptor
                });
                let (ip_allow, ip_deny) = match server.protocol {
                    Protocol::Tcp => (server.ip_allow.clone(), server.ip_deny.clone()),
                    Protocol::Http | Protocol::Udp => (Vec::new(), Vec::new()),
                };
                let context = ConnectionContext {
                    tls,
                    handler,
                    log: ListenerLog {
                        log_name: Arc::clone(&log_name),
                        server: Arc::clone(&label),
                        logger: Arc::clone(&logger),
                    },
                    read_header_timeout: server.read_header_timeout,
                    idle_timeout: server.idle_timeout,
                    proxy_protocol_in: server.proxy_protocol_in,
                    trusted_proxies: server.trusted_proxies.clone(),
                    ip_allow,
                    ip_deny,
                    per_ip: server
                        .max_connections_per_ip
                        .map(|max| (max, Mutex::default())),
                    match_timeout: server.match_timeout,
                };
                (Socket::Tcp(listener), Some(Arc::new(context)))
            }
        };

        let max_connections = server.max_connections.max(1);
        Ok(Self {
            state: watch::channel(State::Starting).0,
            socket,
            address,
            protocol: server.protocol,
            name,
            log_name,
            max_connections,
            permits: Arc::new(Semaphore::new(max_connections)),
            grace_period: config.grace_period,
            context,
            cert_resolver,
            ocsp_stapling: server
                .tls_options
                .as_ref()
                .is_none_or(|options| options.ocsp_stapling),
            announce: true,
            closing,
            shutdown: Box::pin(std::future::pending()),
        })
    }

    /// What another runtime needs to listen on the same address, or `None`
    /// for UDP, which keeps a single socket.
    #[must_use]
    pub fn replica(&self) -> Option<Replica> {
        Some(Replica {
            address: self.address,
            protocol: self.protocol,
            name: self.name.clone(),
            log_name: Arc::clone(&self.log_name),
            max_connections: self.max_connections,
            grace_period: self.grace_period,
            context: Arc::clone(self.context.as_ref()?),
            permits: Arc::clone(&self.permits),
            closing: self.closing.clone(),
        })
    }

    /// Binds one more `SO_REUSEPORT` listener on the address of the server
    /// `replica` comes from, in the current runtime. Both serve the same
    /// routes and count their connections against the same
    /// `max_connections`.
    pub fn from_replica(replica: &Replica) -> Result<Self, Error> {
        let replica = replica.clone();
        Ok(Self {
            state: watch::channel(State::Starting).0,
            socket: Socket::Tcp(bind_tcp(replica.address)?),
            address: replica.address,
            protocol: replica.protocol,
            name: replica.name,
            log_name: replica.log_name,
            max_connections: replica.max_connections,
            permits: replica.permits,
            grace_period: replica.grace_period,
            context: Some(replica.context),
            cert_resolver: None,
            ocsp_stapling: false,
            announce: false,
            closing: replica.closing,
            shutdown: Box::pin(std::future::pending()),
        })
    }

    /// Starts the graceful shutdown when `future` completes.
    #[must_use]
    pub fn shutdown_on(mut self, future: impl Future + Send + 'static) -> Self {
        self.shutdown = Box::pin(async move {
            future.await;
        });
        self
    }

    /// Bound address, useful when listening on port 0.
    #[must_use]
    pub fn socket_address(&self) -> SocketAddr {
        self.address
    }

    /// Protocol of this listener.
    #[must_use]
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Server name from the configuration; the names of the servers sharing
    /// this listener, joined by `, `.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Certificate resolver of a listener that terminates TLS. Certificates
    /// set on it apply to new handshakes right away.
    #[must_use]
    pub fn cert_resolver(&self) -> Option<&Arc<CertResolver>> {
        self.cert_resolver.as_ref()
    }

    /// Receives [`State`] updates.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<State> {
        self.state.subscribe()
    }

    /// Accepts connections until the shutdown future completes, then shuts
    /// down gracefully.
    pub async fn run(self) -> Result<(), Error> {
        let Self {
            state,
            socket,
            log_name,
            max_connections,
            permits,
            grace_period,
            context,
            cert_resolver,
            ocsp_stapling,
            announce,
            closing,
            shutdown,
            ..
        } = self;

        // Dropped, and so aborted, when the listener stops.
        let mut background = JoinSet::new();
        if let Some(resolver) = cert_resolver {
            background.spawn(tls::watch_files(Arc::clone(&resolver)));
            if ocsp_stapling {
                background.spawn(ocsp::run(resolver));
            }
        }

        state.send_replace(State::Listening);
        if announce {
            println!("{log_name} => Listening ({})", self.protocol);
        }

        let (socket, settings) = match socket {
            Socket::Udp(socket, settings) => (socket, settings),
            Socket::Tcp(listener) => {
                let context = context.expect("TCP servers always have a connection context");
                let mut connections = JoinSet::new();
                let (stop, stop_receiver) = watch::channel(false);

                tokio::select! {
                    () = accept_loop(&listener, &state, &permits, max_connections, &mut connections, &context, &stop_receiver) => {}
                    () = shutdown => {
                        if announce {
                            println!("{log_name} => Received shutdown signal");
                        }
                    }
                }

                // Stop accepting: new clients get "connection refused".
                drop(listener);
                closing.send_replace(true);
                while connections.try_join_next().is_some() {}

                let pending = connections.len();
                if pending > 0 {
                    println!("{log_name} => Can't shutdown yet, {pending} pending connections");
                    state.send_replace(State::ShuttingDown(ShutdownState::PendingConnections(
                        pending,
                    )));
                    stop.send_replace(true);

                    let drained = match grace_period {
                        Some(grace) => tokio::time::timeout(grace, drain(&mut connections))
                            .await
                            .is_ok(),
                        None => {
                            drain(&mut connections).await;
                            true
                        }
                    };
                    if !drained {
                        eprintln!(
                            "{log_name} => Grace period over, closing {} connections",
                            connections.len()
                        );
                        connections.abort_all();
                        drain(&mut connections).await;
                    }
                }

                state.send_replace(State::ShuttingDown(ShutdownState::Done));
                if announce {
                    println!("{log_name} => Shutdown complete");
                }
                return Ok(());
            }
        };

        let shutdown = async {
            shutdown.await;
            println!("{log_name} => Received shutdown signal");
        };
        if let Err(err) = udp::serve(socket, settings, shutdown).await {
            eprintln!("{log_name} => UDP socket failed: {err}");
        }

        state.send_replace(State::ShuttingDown(ShutdownState::Done));
        println!("{log_name} => Shutdown complete");
        Ok(())
    }
}

enum SocketKind {
    Tcp(TcpListener),
    Udp(Arc<UdpSocket>),
}

fn bind_tcp(address: SocketAddr) -> io::Result<TcpListener> {
    if let Some(listener) = systemd::tcp_listener(address)? {
        listener.set_nonblocking(true)?;
        return TcpListener::from_std(listener);
    }

    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };

    #[cfg(not(windows))]
    socket.set_reuseaddr(true)?;

    // Lets a reloaded configuration bind the same ports before the old
    // listeners close, so reloads drop no connections.
    #[cfg(unix)]
    socket.set_reuseport(true)?;

    socket.bind(address)?;
    socket.listen(1024)
}

async fn drain(connections: &mut JoinSet<()>) {
    while connections.join_next().await.is_some() {}
}

async fn accept_loop(
    listener: &TcpListener,
    state: &watch::Sender<State>,
    permits: &Arc<Semaphore>,
    max_connections: usize,
    connections: &mut JoinSet<()>,
    context: &Arc<ConnectionContext>,
    stop: &watch::Receiver<bool>,
) {
    loop {
        while connections.try_join_next().is_some() {}

        let full = permits.available_permits() == 0;
        if full {
            println!(
                "{} => Reached max connections: {max_connections}",
                context.log.log_name
            );
            state.send_replace(State::MaxConnectionsReached(max_connections));
        }

        if full {
            // Wait for a free slot without keeping it: holding one while
            // idle in `accept` would starve the replicas of this listener.
            drop(
                permits
                    .acquire()
                    .await
                    .expect("the semaphore is never closed"),
            );
            println!("{} => Accepting connections again", context.log.log_name);
            state.send_replace(State::Listening);
        }

        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Usually EMFILE or ECONNABORTED: back off instead of dying.
                eprintln!("{} => accept failed: {err}", context.log.log_name);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

        // Immediate, unless a replica took the last slot in the meantime.
        let permit = Arc::clone(permits)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");

        let _ = stream.set_nodelay(true);
        let local = stream.local_addr().unwrap_or(peer);
        connections.spawn(handle_connection(
            stream,
            peer,
            local,
            Arc::clone(context),
            stop.clone(),
            permit,
        ));
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    mut peer: SocketAddr,
    local: SocketAddr,
    context: Arc<ConnectionContext>,
    stop: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    let log_name = &context.log.log_name;
    let mut buffer = Vec::new();
    if context.proxy_protocol_in {
        let trusted = &context.trusted_proxies;
        if !trusted.is_empty() && !trusted.iter().any(|range| range.contains(peer.ip())) {
            eprintln!("{log_name} => {peer}: PROXY header from an untrusted peer, closing");
            return;
        }
        match proxy_protocol::read(&mut stream, &mut buffer, context.match_timeout).await {
            Ok(source) => peer = source.unwrap_or(peer),
            Err(err) => {
                eprintln!("{log_name} => {peer}: invalid PROXY header: {err}");
                return;
            }
        }
    }
    if !net::permitted(peer.ip(), &context.ip_allow, &context.ip_deny) {
        eprintln!("{log_name} => {peer}: rejected by ip_allow or ip_deny, closing");
        return;
    }
    // Closed without a log line: a client over its limit could flood it.
    let _slot = match &context.per_ip {
        Some((max, counts)) => match PerIpSlot::acquire(*max, counts, peer.ip()) {
            Some(slot) => Some(slot),
            None => return,
        },
        None => None,
    };
    let stream = Rewind::new(buffer, stream);

    match &context.tls {
        Some(acceptor) => match tokio::time::timeout(
            context
                .read_header_timeout
                .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT),
            acceptor.accept(stream),
        )
        .await
        {
            Err(_) => eprintln!("{log_name} => TLS handshake with {peer} timed out"),
            Ok(Ok(stream)) => {
                let (_, connection) = stream.get_ref();
                // A TLS-ALPN-01 validation ends with the handshake.
                if connection.alpn_protocol() == Some(acme::TLS_ALPN_PROTOCOL) {
                    return;
                }
                let client_subject = tls::client_subject(connection);
                let handshake = Detected {
                    tls: true,
                    sni: connection.server_name().map(str::to_owned),
                    alpn: connection
                        .alpn_protocol()
                        .map(<[u8]>::to_vec)
                        .into_iter()
                        .collect(),
                    ..Detected::default()
                };
                serve(
                    stream,
                    peer,
                    local,
                    &context,
                    stop,
                    Some(handshake),
                    client_subject,
                )
                .await;
            }
            Ok(Err(err)) => eprintln!("{log_name} => TLS handshake with {peer} failed: {err}"),
        },
        None => serve(stream, peer, local, &context, stop, None, None).await,
    }
}

/// Runs the handler of `context` on an accepted (and maybe decrypted)
/// connection. `handshake` holds what the TLS handshake revealed, and
/// `client_subject` the subject of the client certificate.
async fn serve<S>(
    mut stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    context: &ConnectionContext,
    stop: watch::Receiver<bool>,
    handshake: Option<Detected>,
    client_subject: Option<HeaderValue>,
) where
    S: tcp::Client + Send + 'static,
{
    match &context.handler {
        Handler::Tcp {
            routes,
            sniff,
            proxy_protocol,
            limits,
        } => {
            let log_name = &context.log.log_name;
            let mut buffer = Vec::new();
            let mut detected = if *sniff {
                match l4match::sniff(&mut stream, &mut buffer, context.match_timeout).await {
                    Ok(detected) => detected,
                    Err(err) => {
                        eprintln!("{log_name} => {peer}: reading the first bytes failed: {err}");
                        return;
                    }
                }
            } else {
                Detected::default()
            };
            if let Some(handshake) = handshake {
                detected.tls = true;
                detected.sni = handshake.sni;
                detected.alpn = handshake.alpn;
            }
            let Some(route) = routes.iter().find(|route| route.matcher.matches(&detected)) else {
                eprintln!("{log_name} => {peer}: no match entry accepts the connection, closing");
                return;
            };
            tcp::proxy(
                Rewind::new(buffer, stream),
                peer,
                local,
                route.split.pick(),
                *proxy_protocol,
                *limits,
                &context.log,
            )
            .await;
        }
        Handler::Http { routes, http2 } => {
            let activity = Arc::new(Activity::default());
            let service = Service {
                routes: Arc::clone(routes),
                log_name: Arc::clone(&context.log.log_name),
                peer,
                peer_ip: headers::peer_ip_value(peer),
                client_subject,
                local,
                activity: Arc::clone(&activity),
                logger: Arc::clone(&context.log.logger),
            };
            let stamp = Stamp::new();
            let io = TokioIo::new(Watched::new(stream, Arc::clone(&stamp)));
            let idle = context.idle_timeout.map(|idle| (idle, stamp));
            let first_request = context
                .read_header_timeout
                .unwrap_or(DEFAULT_READ_HEADER_TIMEOUT);

            let result = if *http2 {
                let mut builder = auto::Builder::new(TokioExecutor::new());
                let mut http1 = builder.http1();
                http1.preserve_header_case(true).timer(TokioTimer::new());
                if let Some(timeout) = context.read_header_timeout {
                    http1.header_read_timeout(timeout);
                }
                drive_http(
                    builder.serve_connection_with_upgrades(io, service),
                    stop,
                    &activity,
                    first_request,
                    idle,
                )
                .await
            } else {
                let mut builder = hyper::server::conn::http1::Builder::new();
                builder.preserve_header_case(true).timer(TokioTimer::new());
                if let Some(timeout) = context.read_header_timeout {
                    builder.header_read_timeout(timeout);
                }
                let connection = builder.serve_connection(io, service).with_upgrades();
                drive_http(connection, stop, &activity, first_request, idle).await
            };

            if let Err(err) = result {
                eprintln!("{} => connection from {peer}: {err}", context.log.log_name);
            }
        }
    }
}

/// Graceful shutdown hook shared by the HTTP/1 and auto connection types.
trait GracefulConnection: Future {
    fn begin_graceful_shutdown(self: Pin<&mut Self>);
}

impl<I, S> GracefulConnection for hyper::server::conn::http1::UpgradeableConnection<I, S>
where
    Self: Future,
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    S: hyper::service::HttpService<hyper::body::Incoming>,
    S::ResBody: hyper::body::Body + 'static,
    <S::ResBody as hyper::body::Body>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    fn begin_graceful_shutdown(self: Pin<&mut Self>) {
        self.graceful_shutdown();
    }
}

impl<I, S, E> GracefulConnection for auto::UpgradeableConnection<'_, I, S, E>
where
    Self: Future,
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    S: hyper::service::HttpService<hyper::body::Incoming>,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    S::ResBody: hyper::body::Body + Send + 'static,
    <S::ResBody as hyper::body::Body>::Data: Send,
    <S::ResBody as hyper::body::Body>::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    E: hyper::rt::bounds::Http2ServerConnExec<S::Future, S::ResBody>,
{
    fn begin_graceful_shutdown(self: Pin<&mut Self>) {
        self.graceful_shutdown();
    }
}

/// Runs an HTTP connection. A connection that sends no complete request head
/// within `first_request` is dropped: hyper's own header timer does not run
/// while the HTTP/2 capable builder waits for the first bytes, nor before the
/// first byte of an HTTP/1 request. A connection without traffic and without
/// requests in flight for `idle` is dropped. Once shutdown starts, the
/// connection is closed after its current request; connections that never
/// sent a request are left open until they send one (within `first_request`),
/// so clients that connected right before the shutdown still get an answer.
async fn drive_http<C, E>(
    connection: C,
    mut stop: watch::Receiver<bool>,
    activity: &Activity,
    first_request: Duration,
    idle: Option<(Duration, Arc<Stamp>)>,
) -> Result<(), String>
where
    C: GracefulConnection<Output = Result<(), E>>,
    E: std::fmt::Display,
{
    let mut connection = std::pin::pin!(connection);

    let idle = async {
        match &idle {
            Some((limit, stamp)) => stamp.idle_for(*limit, || activity.in_flight() > 0).await,
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        result = connection.as_mut() => return result.map_err(|err| err.to_string()),
        () = idle => return Ok(()),
        () = async {
            if tokio::time::timeout(first_request, activity.first_request()).await.is_ok() {
                std::future::pending::<()>().await;
            }
        } => return Err(format!("no request head within {first_request:?}")),
        () = async {
            let _ = stop.wait_for(|stopping| *stopping).await;
            activity.first_request().await;
        } => {}
    }

    connection.as_mut().begin_graceful_shutdown();
    connection.await.map_err(|err| err.to_string())
}
