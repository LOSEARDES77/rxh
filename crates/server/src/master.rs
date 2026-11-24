//! Spawns every listener of a configuration and shuts them down together.
//!
//! A server with several `listen` addresses becomes one [`Server`] per
//! address ("replicas"), and HTTP servers listing the same address (other
//! than port 0) share one [`Server`], so each socket has exactly one owner:
//!
//! ```text
//!                 shutdown signal
//!                       |
//!                   +--------+
//!                   | Master |
//!                   +--------+
//!                  /          \
//!          +----------+   +----------+
//!          | Server 1 |   | Server 2 |   one per listen address
//!          +----------+   +----------+
//!            /     \         /     \
//!         conn    conn    conn    conn   one task per connection
//! ```

use std::collections::BTreeMap;
use std::future::{self, Future};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Weak};

use rxh_core::config::{Config, Protocol};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, watch};

use crate::access::Host;
use crate::error::Error;
use crate::http3;
use crate::registry::Registry;
use crate::server::{Replica, Server, State};
use crate::tls::CertResolver;

/// Description of a running listener, for status reports.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Bound address.
    pub address: SocketAddr,

    /// Protocol.
    pub protocol: Protocol,

    /// Name from the configuration; the names of the servers sharing the
    /// socket, joined by `, `.
    pub name: Option<String>,

    /// Live state.
    pub state: watch::Receiver<State>,

    /// Certificates of a listener that terminates TLS.
    pub cert_resolver: Option<Arc<CertResolver>>,

    /// What the listener serves over HTTP/3, if it does.
    pub(crate) http3: Option<Arc<http3::Listener>>,

    /// Name and groups of each server sharing the socket, which decide who
    /// sees the listener in the admin API. Empty for a replica.
    pub(crate) hosts: Vec<Host>,
}

/// Live TCP sockets whose accept queues can be carried into another generation.
pub(crate) type TcpListeners = BTreeMap<SocketAddr, Weak<TcpListener>>;

/// Owner of all [`Server`]s of one configuration.
pub struct Master {
    servers: Vec<Server>,
    infos: Vec<ServerInfo>,
    /// Members (indexes into `config.servers`) and resolver of every TLS
    /// listener.
    resolvers: Vec<(Vec<usize>, Arc<CertResolver>)>,
    registry: Arc<Registry>,
    /// False for replicas, which do not repeat the messages of the first
    /// master.
    announce: bool,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    shutdown_notify: broadcast::Sender<()>,
}

/// Groups listen addresses into sockets: HTTP servers sharing an address
/// (port 0 excluded) get one socket, in configuration order.
fn listeners(config: &Config) -> Vec<(SocketAddr, Vec<usize>)> {
    let mut listeners: Vec<(SocketAddr, Vec<usize>)> = Vec::new();
    for (index, server) in config.servers.iter().enumerate() {
        let shareable = server.protocol == Protocol::Http;
        for &address in &server.listen {
            // ponytail: linear scan, configurations have few listeners.
            let shared = listeners.iter_mut().find(|(existing, members)| {
                shareable
                    && address.port() != 0
                    && *existing == address
                    && config.servers[members[0]].protocol == Protocol::Http
            });
            match shared {
                Some((_, members)) if members.contains(&index) => {}
                Some((_, members)) => members.push(index),
                None => listeners.push((address, vec![index])),
            }
        }
    }
    listeners
}

impl Master {
    /// Binds every listener of `config`, using the pools of `registry`.
    pub fn init(config: &Config, registry: Arc<Registry>) -> Result<Self, Error> {
        Self::init_with_tcp(config, registry, &[])
    }

    pub(crate) fn init_with_tcp(
        config: &Config,
        registry: Arc<Registry>,
        previous: &[TcpListeners],
    ) -> Result<Self, Error> {
        let mut servers = Vec::new();
        let mut infos = Vec::new();
        let mut resolvers = Vec::new();

        for (address, members) in listeners(config) {
            let server = Server::init_with_tcp(
                config,
                &registry,
                address,
                &members,
                previous
                    .first()
                    .and_then(|listeners| listeners.get(&address)),
            )?;
            let hosts: Vec<Host> = members
                .iter()
                .map(|&member| Host::from(&config.servers[member]))
                .collect();
            if let Some(resolver) = server.cert_resolver() {
                resolvers.push((members, Arc::clone(resolver)));
            }
            infos.push(ServerInfo {
                address: server.socket_address(),
                protocol: server.protocol(),
                name: server.name().map(str::to_owned),
                state: server.subscribe(),
                cert_resolver: server.cert_resolver().cloned(),
                http3: server.http3().cloned(),
                hosts,
            });
            servers.push(server);
        }

        // Joining per-core listeners onto the shared runtime must keep
        // their accept queues too. They serve the primary listener's routes
        // and count against its connection limit.
        let replicas: Vec<_> = servers.iter().filter_map(Server::replica).collect();
        for listeners in previous.iter().skip(1) {
            for replica in &replicas {
                if let Some(listener) = listeners.get(&replica.address) {
                    servers.push(Server::from_replica_with_tcp(replica, Some(listener))?);
                }
            }
        }

        Ok(Self {
            servers,
            infos,
            resolvers,
            registry,
            announce: true,
            shutdown: Box::pin(future::pending()),
            shutdown_notify: broadcast::channel(1).0,
        })
    }

    pub(crate) fn tcp_listeners(&self) -> Vec<TcpListeners> {
        let mut listeners = vec![TcpListeners::new()];
        for server in &self.servers {
            let Some(listener) = server.tcp_listener() else {
                continue;
            };
            let address = server.socket_address();
            if let Some(group) = listeners
                .iter_mut()
                .find(|group| !group.contains_key(&address))
            {
                group.insert(address, listener);
            } else {
                listeners.push(TcpListeners::from([(address, listener)]));
            }
        }
        listeners
    }

    /// What [`Master::from_replicas`] needs to run the same listeners in
    /// another runtime. UDP listeners are left out: they keep one socket.
    #[must_use]
    pub fn replicas(&self) -> Vec<Replica> {
        self.servers[..self.infos.len()]
            .iter()
            .filter_map(Server::replica)
            .collect()
    }

    /// Binds one more listener per entry of `replicas`, in the current
    /// runtime, next to the ones of the [`Master`] they come from.
    pub fn from_replicas(replicas: &[Replica], registry: Arc<Registry>) -> Result<Self, Error> {
        Self::from_replicas_with_tcp(replicas, registry, None)
    }

    pub(crate) fn from_replicas_with_tcp(
        replicas: &[Replica],
        registry: Arc<Registry>,
        previous: Option<&TcpListeners>,
    ) -> Result<Self, Error> {
        let mut servers = Vec::new();
        let mut infos = Vec::new();
        for replica in replicas {
            let server = Server::from_replica_with_tcp(
                replica,
                previous.and_then(|listeners| listeners.get(&replica.address)),
            )?;
            infos.push(ServerInfo {
                address: server.socket_address(),
                protocol: server.protocol(),
                name: server.name().map(str::to_owned),
                state: server.subscribe(),
                cert_resolver: None,
                http3: None,
                hosts: Vec::new(),
            });
            servers.push(server);
        }
        Ok(Self {
            servers,
            infos,
            resolvers: Vec::new(),
            registry,
            announce: false,
            shutdown: Box::pin(future::pending()),
            shutdown_notify: broadcast::channel(1).0,
        })
    }

    /// Validates `config`, builds its pools and binds its listeners.
    pub fn from_config(config: &Config) -> Result<Self, Error> {
        config.validate()?;
        let config = &config.with_https_redirects();
        let registry = Arc::new(Registry::build(config)?);
        Self::init(config, registry)
    }

    /// Pools used by this configuration.
    #[must_use]
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Certificate resolvers of the TLS listeners that `config.servers[index]`
    /// listens on, one per listen address.
    #[must_use]
    pub fn cert_resolvers(&self, index: usize) -> Vec<Arc<CertResolver>> {
        self.resolvers
            .iter()
            .filter(|(members, _)| members.contains(&index))
            .map(|(_, resolver)| Arc::clone(resolver))
            .collect()
    }

    /// Starts the graceful shutdown of every server when `future` completes.
    #[must_use]
    pub fn shutdown_on(mut self, future: impl Future + Send + 'static) -> Self {
        self.servers = self
            .servers
            .into_iter()
            .map(|server| {
                let mut notification = self.shutdown_notify.subscribe();
                server.shutdown_on(async move { notification.recv().await })
            })
            .collect();

        self.shutdown = Box::pin(async move {
            future.await;
        });

        self
    }

    /// Bound addresses of all listeners.
    #[must_use]
    pub fn sockets(&self) -> Vec<SocketAddr> {
        self.infos.iter().map(|info| info.address).collect()
    }

    /// Descriptions of all listeners.
    #[must_use]
    pub fn servers(&self) -> Vec<ServerInfo> {
        self.infos.clone()
    }

    /// Runs every server until the shutdown future completes or a server
    /// fails, then waits for all of them to finish.
    pub async fn run(self) -> Result<(), Error> {
        let announce = self.announce;
        let mut set = tokio::task::JoinSet::new();
        for server in self.servers {
            set.spawn(server.run());
        }

        let mut first_error = None;

        tokio::select! {
            Some(Ok(Err(err))) = set.join_next() => {
                eprintln!("Master => A server failed, shutting down the rest: {err}");
                first_error = Some(err);
            }
            () = self.shutdown => {
                if announce {
                    println!("Master => Sending shutdown signal to all servers");
                }
            }
        }

        let _ = self.shutdown_notify.send(());

        while let Some(result) = set.join_next().await {
            match result {
                Ok(Err(err)) => {
                    first_error.get_or_insert(err);
                }
                Err(join_error) if join_error.is_panic() => {
                    eprintln!("Master => A server panicked: {join_error}");
                }
                _ => {}
            }
        }

        first_error.map_or(Ok(()), Err)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use super::*;

    #[tokio::test]
    async fn replacement_keeps_queued_connections_from_every_listener() {
        let config = |address, location| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"{address}\"\nredirect = \"{location}\"\n"
            ))
            .expect("valid configuration")
        };

        let initial = Master::from_config(&config("127.0.0.1:0", "/old")).expect("first listener");
        let replica = Master::from_replicas(&initial.replicas(), Arc::clone(initial.registry()))
            .expect("replica listener");
        let address = initial.sockets()[0];

        // These connect before a replacement can bind. Neither old acceptor
        // has started, so the requests remain queued on the original sockets.
        let mut clients = Vec::new();
        for _ in 0..32 {
            let mut client = TcpStream::connect(address)
                .await
                .expect("queued connection");
            client
                .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
                .await
                .expect("write queued request");
            clients.push(client);
        }

        let previous = [initial.tcp_listeners(), replica.tcp_listeners()].concat();
        let replacement = config(&address.to_string(), "/new");
        let registry = Arc::new(Registry::build(&replacement).expect("new registry"));
        let master = Master::init_with_tcp(&replacement, registry, &previous).expect("handoff");
        drop(initial);
        drop(replica);

        let running = tokio::spawn(master.run());
        for mut client in clients {
            let mut response = String::new();
            tokio::time::timeout(Duration::from_secs(2), client.read_to_string(&mut response))
                .await
                .expect("replacement answered")
                .expect("connection survived");
            assert!(response.contains("location: /new\r\n"), "{response}");
        }
        running.abort();
    }
}
