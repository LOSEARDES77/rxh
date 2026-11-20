//! The daemon's QUIC endpoints, one UDP socket per address, which outlive
//! generations.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use quinn::{AsyncUdpSocket, Endpoint, EndpointConfig, Runtime, TokioRuntime};
use rxh_core::config::Config;
use socket2::{Domain, Socket, Type};
use tokio::sync::watch;

use super::connection;
use super::listener::{self, Listener};
use crate::error::Error;
use crate::systemd;

/// Longest wait in [`Endpoints::release`] for a closed socket to go away.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

/// Endpoints by listen address.
type Map = Mutex<HashMap<SocketAddr, Entry>>;

/// QUIC endpoints of the daemon, by listen address. They outlive
/// generations.
#[derive(Default)]
pub(crate) struct Endpoints {
    /// Shared with the tasks that drop retired endpoints once they are idle.
    endpoints: Arc<Map>,
}

/// One endpoint and the listener it hands new connections to.
struct Entry {
    endpoint: Endpoint,
    /// Dropping it ends the accept loop.
    listener: watch::Sender<Arc<Listener>>,
    /// The UDP socket, gone once the endpoint, its driver and its
    /// connections are.
    socket: Weak<dyn AsyncUdpSocket>,
    /// Set while no generation serves the address: the endpoint takes no new
    /// connection and stays here until its open connections are gone, so
    /// the address never gets a second socket.
    retired: bool,
}

impl Endpoints {
    /// Makes `listeners` the ones new connections are served by, binding
    /// the addresses that have no endpoint yet and reviving retired ones.
    /// Every new address is bound before anything changes, so on error the
    /// endpoints serve what they served before. Endpoints on addresses
    /// `listeners` has none for are retired: they take no new connection,
    /// and close once the generation that served them has drained its
    /// connections.
    pub(crate) fn install(&self, listeners: &[Arc<Listener>]) -> Result<(), Error> {
        let mut endpoints = self
            .endpoints
            .lock()
            .expect("nothing panics while holding the endpoints");

        let mut bound = Vec::new();
        for listener in listeners {
            if endpoints.contains_key(&listener.address) {
                continue;
            }
            match bind(listener) {
                Ok((endpoint, socket)) => bound.push((listener, endpoint, socket)),
                Err(err) if listener.requires_http3() => {
                    let address = listener.address;
                    return Err(io::Error::new(
                        err.kind(),
                        format!("HTTP/3 on {address} (UDP): {err}"),
                    )
                    .into());
                }
                Err(err) => {
                    let log_name = &listener.context.log.log_name;
                    eprintln!(
                        "{log_name} => HTTP/3 disabled: cannot bind UDP {}: {err}",
                        listener.address
                    );
                    listener.set_active(false);
                }
            }
        }

        let addresses: HashSet<SocketAddr> =
            listeners.iter().map(|listener| listener.address).collect();
        for (address, entry) in endpoints.iter_mut() {
            if addresses.contains(address) || entry.retired {
                continue;
            }
            entry.retired = true;
            entry.listener.borrow().set_active(false);
            entry.endpoint.set_server_config(None);
            tokio::task::spawn(remove_when_idle(
                Arc::downgrade(&self.endpoints),
                *address,
                entry.endpoint.clone(),
                Weak::clone(&entry.socket),
            ));
        }
        for listener in listeners {
            if let Some(entry) = endpoints.get_mut(&listener.address) {
                entry.retired = false;
                entry
                    .endpoint
                    .set_server_config(Some(listener.config.clone()));
                // Before the new one is set: it may be the same listener.
                entry
                    .listener
                    .send_replace(Arc::clone(listener))
                    .set_active(false);
                listener.set_active(true);
            }
        }
        for (listener, endpoint, socket) in bound {
            listener.set_active(true);
            let (sender, receiver) = watch::channel(Arc::clone(listener));
            tokio::task::spawn(accept(endpoint.clone(), receiver));
            let entry = Entry {
                endpoint,
                listener: sender,
                socket,
                retired: false,
            };
            endpoints.insert(listener.address, entry);
        }
        Ok(())
    }

    /// Closes at once the endpoints, retired ones included, on addresses
    /// `config` gives to a `udp` server, and waits until their sockets are
    /// closed, so that server can bind them. A server on `0.0.0.0` takes
    /// its port on every address.
    pub(crate) async fn release(&self, config: &Config) {
        let mut released = Vec::new();
        self.endpoints
            .lock()
            .expect("nothing panics while holding the endpoints")
            .retain(|address, entry| {
                if !config.udp_listens_on(*address) {
                    return true;
                }
                entry.listener.borrow().set_active(false);
                entry.endpoint.close(listener::no_error(), b"");
                released.push((*address, Weak::clone(&entry.socket)));
                false
            });

        // ponytail: polls every 10 ms, the socket has no close event. On
        // timeout the `udp` server fails to bind and the reload stops the
        // running generation first.
        let closed = async {
            while released.iter().any(|(_, socket)| socket.strong_count() > 0) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        if tokio::time::timeout(RELEASE_TIMEOUT, closed).await.is_err() {
            for (address, socket) in &released {
                if socket.strong_count() > 0 {
                    eprintln!(
                        "Main => HTTP/3 on {address} (UDP): the socket is still open after {RELEASE_TIMEOUT:?}"
                    );
                }
            }
        }
    }

    /// Closes every endpoint and its connections at once: when the daemon
    /// stops, and at a hot binary upgrade, whose new process binds its own
    /// sockets.
    pub(crate) fn close(&self) {
        let mut endpoints = self
            .endpoints
            .lock()
            .expect("nothing panics while holding the endpoints");
        for (_, entry) in endpoints.drain() {
            entry.listener.borrow().set_active(false);
            entry.endpoint.close(listener::no_error(), b"");
        }
    }
}

/// Drops the retired endpoint on `address`, the one over `socket`, from
/// `endpoints` once it has no connection left. Its socket then closes.
/// Nothing is dropped if the endpoint was revived meanwhile.
async fn remove_when_idle(
    endpoints: Weak<Map>,
    address: SocketAddr,
    endpoint: Endpoint,
    socket: Weak<dyn AsyncUdpSocket>,
) {
    endpoint.wait_idle().await;
    let Some(endpoints) = endpoints.upgrade() else {
        return;
    };
    let mut endpoints = endpoints
        .lock()
        .expect("nothing panics while holding the endpoints");
    let idle = endpoints
        .get(&address)
        .is_some_and(|entry| entry.retired && Weak::ptr_eq(&entry.socket, &socket));
    if idle {
        endpoints.remove(&address);
    }
}

/// QUIC endpoint on the address of `listener`, over the UDP socket systemd
/// passed for it or a new one, and that socket. Runs on the calling
/// runtime.
fn bind(listener: &Listener) -> io::Result<(Endpoint, Weak<dyn AsyncUdpSocket>)> {
    let address = listener.address;
    let socket = match systemd::udp_socket(address)? {
        Some(socket) => socket,
        None => {
            let socket = Socket::new(Domain::for_address(address), Type::DGRAM, None)?;
            // Lets the next process of a hot binary upgrade bind the port
            // while this one still serves it.
            #[cfg(unix)]
            socket.set_reuse_port(true)?;
            // IPv6 only or not as the OS decides, like the TCP listener on
            // the address: `[::]` serves IPv4 clients too where TCP does.
            socket.bind(&address.into())?;
            socket.into()
        }
    };
    socket.set_nonblocking(true)?;
    let socket = TokioRuntime.wrap_udp_socket(socket)?;
    let weak = Arc::downgrade(&socket);
    let endpoint = Endpoint::new_with_abstract_socket(
        EndpointConfig::default(),
        Some(listener.config.clone()),
        socket,
        Arc::new(TokioRuntime),
    )?;
    Ok((endpoint, weak))
}

/// Accepts the connections of `endpoint`, each one served by the listener
/// current when it arrives, until the endpoint is closed or retired.
async fn accept(endpoint: Endpoint, mut listener: watch::Receiver<Arc<Listener>>) {
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    return;
                };
                let listener = Arc::clone(&listener.borrow());
                tokio::task::spawn(connection::serve_connection(incoming, listener));
            }
            // Retired: the entry, and the sender with it, is gone.
            changed = listener.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
    }
}
