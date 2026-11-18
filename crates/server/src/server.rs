//! One listening socket and the connections it accepts.
//!
//! A `[[server]]` with several `listen` addresses becomes several [`Server`]
//! values, and HTTP servers listing the same address share one [`Server`]
//! (see [`crate::Master`]). Each one accepts connections, limits them
//! to `max_connections`, and on shutdown stops accepting, lets open
//! connections finish (up to `grace_period`) and reports progress through
//! [`State`].

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto;
use rxh_core::config::{Action, Config, Protocol, ProxyProtocol};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;

use crate::error::Error;
use crate::http::service::{Activity, Route, Service, Site};
use crate::registry::{Group, Registry};
use crate::tcp;
use crate::timeout::{Stamp, Watched};
use crate::tls;
use crate::udp::{self, UdpSettings};

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
        group: Arc<Group>,
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
    log_name: Arc<str>,
    read_header_timeout: Option<Duration>,
    idle_timeout: Option<Duration>,
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
    grace_period: Option<Duration>,
    context: Option<Arc<ConnectionContext>>,
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
                let socket = std::net::UdpSocket::bind(requested)?;
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

        let forward_group = || -> Result<Arc<Group>, Error> {
            let Action::Forward(forward) = &server.patterns[0].action else {
                unreachable!("validation guarantees L4 servers forward")
            };
            registry.resolve(forward, server_index, server, 0)
        };

        let (socket, context) = match socket {
            SocketKind::Udp(socket) => {
                let settings = UdpSettings {
                    group: forward_group()?,
                    idle_timeout: server.udp_idle_timeout,
                    max_sessions: server.max_connections,
                    log_name: Arc::clone(&log_name),
                };
                (Socket::Udp(socket, settings), None)
            }
            SocketKind::Tcp(listener) => {
                let handler = match server.protocol {
                    Protocol::Tcp => Handler::Tcp {
                        group: forward_group()?,
                        proxy_protocol: server.proxy_protocol,
                        limits: tcp::Limits {
                            idle: server.idle_timeout,
                            stream: server.stream_timeout,
                        },
                    },
                    _ => {
                        let mut routes = Vec::new();
                        for (&index, member) in members.iter().zip(&servers) {
                            Site::build_routes(
                                index,
                                member,
                                registry,
                                site_log_name(member.name.as_deref()),
                                address,
                                &mut routes,
                            )?;
                        }
                        Handler::Http {
                            routes: routes.into(),
                            http2: server.http2,
                        }
                    }
                };
                let context = ConnectionContext {
                    tls: tls::acceptor(&servers)?,
                    handler,
                    log_name: Arc::clone(&log_name),
                    read_header_timeout: server.read_header_timeout,
                    idle_timeout: server.idle_timeout,
                };
                (Socket::Tcp(listener), Some(Arc::new(context)))
            }
        };

        Ok(Self {
            state: watch::channel(State::Starting).0,
            socket,
            address,
            protocol: server.protocol,
            name,
            log_name,
            max_connections: server.max_connections.max(1),
            grace_period: config.grace_period,
            context,
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
            grace_period,
            context,
            shutdown,
            ..
        } = self;

        state.send_replace(State::Listening);
        println!("{log_name} => Listening ({})", self.protocol);

        let (socket, settings) = match socket {
            Socket::Udp(socket, settings) => (socket, settings),
            Socket::Tcp(listener) => {
                let context = context.expect("TCP servers always have a connection context");
                let mut connections = JoinSet::new();
                let (stop, stop_receiver) = watch::channel(false);
                let permits = Arc::new(Semaphore::new(max_connections));

                tokio::select! {
                    () = accept_loop(&listener, &state, &permits, max_connections, &mut connections, &context, &stop_receiver) => {}
                    () = shutdown => println!("{log_name} => Received shutdown signal"),
                }

                // Stop accepting: new clients get "connection refused".
                drop(listener);
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
                println!("{log_name} => Shutdown complete");
                return Ok(());
            }
        };

        tokio::select! {
            result = udp::serve(socket, settings) => {
                if let Err(err) = result {
                    eprintln!("{log_name} => UDP socket failed: {err}");
                }
            }
            () = shutdown => println!("{log_name} => Received shutdown signal"),
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
                context.log_name
            );
            state.send_replace(State::MaxConnectionsReached(max_connections));
        }

        let permit = Arc::clone(permits)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");

        if full {
            println!("{} => Accepting connections again", context.log_name);
            state.send_replace(State::Listening);
        }

        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) => {
                // Usually EMFILE or ECONNABORTED: back off instead of dying.
                eprintln!("{} => accept failed: {err}", context.log_name);
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };

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
    stream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    context: Arc<ConnectionContext>,
    stop: watch::Receiver<bool>,
    _permit: OwnedSemaphorePermit,
) {
    match &context.tls {
        Some(acceptor) => match acceptor.accept(stream).await {
            Ok(stream) => serve(stream, peer, local, &context, stop).await,
            Err(err) => eprintln!(
                "{} => TLS handshake with {peer} failed: {err}",
                context.log_name
            ),
        },
        None => serve(stream, peer, local, &context, stop).await,
    }
}

async fn serve<S>(
    stream: S,
    peer: SocketAddr,
    local: SocketAddr,
    context: &ConnectionContext,
    stop: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    match &context.handler {
        Handler::Tcp {
            group,
            proxy_protocol,
            limits,
        } => {
            tcp::proxy(
                stream,
                peer,
                local,
                group,
                *proxy_protocol,
                *limits,
                &context.log_name,
            )
            .await;
        }
        Handler::Http { routes, http2 } => {
            let activity = Arc::new(Activity::default());
            let service = Service {
                routes: Arc::clone(routes),
                log_name: Arc::clone(&context.log_name),
                peer,
                local,
                activity: Arc::clone(&activity),
            };
            let stamp = Stamp::new();
            let io = TokioIo::new(Watched::new(stream, Arc::clone(&stamp)));
            let idle = context.idle_timeout.map(|idle| (idle, stamp));

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
                drive_http(connection, stop, &activity, idle).await
            };

            if let Err(err) = result {
                eprintln!("{} => connection from {peer}: {err}", context.log_name);
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

/// Runs an HTTP connection. A connection without traffic and without requests
/// in flight for `idle` is dropped. Once shutdown starts, the connection is closed
/// after its current request; connections that never sent a request are left
/// open until they send one, so clients that connected right before the
/// shutdown still get an answer.
async fn drive_http<C, E>(
    connection: C,
    mut stop: watch::Receiver<bool>,
    activity: &Activity,
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
            let _ = stop.wait_for(|stopping| *stopping).await;
            activity.first_request().await;
        } => {}
    }

    connection.as_mut().begin_graceful_shutdown();
    connection.await.map_err(|err| err.to_string())
}
