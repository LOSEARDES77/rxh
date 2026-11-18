//! Spawns every listener of a configuration and shuts them down together.
//!
//! A server with several `listen` addresses becomes one [`Server`] per
//! address ("replicas"), so each server owns exactly one socket:
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

use std::future::{self, Future};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use rxh_core::config::{Config, Protocol};
use tokio::sync::{broadcast, watch};

use crate::error::Error;
use crate::registry::Registry;
use crate::server::{Server, State};

/// Description of a running listener, for status reports.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    /// Bound address.
    pub address: SocketAddr,

    /// Protocol.
    pub protocol: Protocol,

    /// Name from the configuration.
    pub name: Option<String>,

    /// Live state.
    pub state: watch::Receiver<State>,
}

/// Owner of all [`Server`]s of one configuration.
pub struct Master {
    servers: Vec<Server>,
    infos: Vec<ServerInfo>,
    registry: Arc<Registry>,
    shutdown: Pin<Box<dyn Future<Output = ()> + Send>>,
    shutdown_notify: broadcast::Sender<()>,
}

impl Master {
    /// Binds every listener of `config`, using the pools of `registry`.
    pub fn init(config: &Config, registry: Arc<Registry>) -> Result<Self, Error> {
        let mut servers = Vec::new();
        let mut infos = Vec::new();

        for (server_index, server_config) in config.servers.iter().enumerate() {
            for replica in 0..server_config.listen.len() {
                let server = Server::init(config, &registry, server_index, replica)?;
                infos.push(ServerInfo {
                    address: server.socket_address(),
                    protocol: server.protocol(),
                    name: server.name().map(str::to_owned),
                    state: server.subscribe(),
                });
                servers.push(server);
            }
        }

        Ok(Self {
            servers,
            infos,
            registry,
            shutdown: Box::pin(future::pending()),
            shutdown_notify: broadcast::channel(1).0,
        })
    }

    /// Validates `config`, builds its pools and binds its listeners.
    pub fn from_config(config: &Config) -> Result<Self, Error> {
        config.validate()?;
        let registry = Arc::new(Registry::build(config)?);
        Self::init(config, registry)
    }

    /// Pools used by this configuration.
    #[must_use]
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
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
            () = self.shutdown => println!("Master => Sending shutdown signal to all servers"),
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
