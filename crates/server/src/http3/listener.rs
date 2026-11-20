//! What one generation serves over HTTP/3 on one address, and how its
//! connections are drained when the generation stops.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use h3::error::Code;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{IdleTimeout, TransportConfig, VarInt};
use tokio::sync::{Semaphore, watch};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::quic::Suite;
use tokio_rustls::rustls::{CipherSuite, ServerConfig};

use crate::server::ConnectionContext;
use crate::tls;

/// What the connections of a [`Listener`] are asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// Serve requests.
    Serving,
    /// Send GOAWAY, finish the requests in flight, then close.
    Draining,
    /// Close now.
    Closed,
}

/// What one generation serves over HTTP/3 on one address.
pub(crate) struct Listener {
    pub(super) address: SocketAddr,
    /// QUIC and TLS settings of new connections.
    pub(super) config: quinn::ServerConfig,
    /// Routes, limits and log, shared with the TCP listener.
    pub(super) context: Arc<ConnectionContext>,
    /// `max_connections`: one permit per open connection, shared with the
    /// TCP listener.
    pub(super) permits: Arc<Semaphore>,
    /// Tells the open connections to drain, then to close.
    pub(super) phase: watch::Sender<Phase>,
    /// Connections accepted and not closed yet.
    open: watch::Sender<usize>,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Listener on `address`, the address of the TCP listener whose
    /// `context` and connection `permits` it shares, with the certificates
    /// and client verifier of its TLS acceptor `tls`.
    pub(crate) fn new(
        address: SocketAddr,
        tls: &TlsAcceptor,
        context: Arc<ConnectionContext>,
        permits: Arc<Semaphore>,
    ) -> io::Result<Self> {
        let crypto =
            QuicServerConfig::with_initial(tls::quic_config(tls.config()), initial_suite()?)
                .map_err(io::Error::other)?;
        let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
        // A timeout too large for QUIC keeps quinn's default.
        if let Some(idle) = context.idle_timeout
            && let Ok(idle) = IdleTimeout::try_from(idle)
        {
            let mut transport = TransportConfig::default();
            transport.max_idle_timeout(Some(idle));
            config.transport_config(Arc::new(transport));
        }
        Ok(Self {
            address,
            config,
            context,
            permits,
            phase: watch::Sender::new(Phase::Serving),
            open: watch::Sender::new(0),
        })
    }

    /// Counts one more open connection, until the guard is dropped.
    pub(super) fn count_open(&self) -> Open<'_> {
        self.open.send_modify(|open| *open += 1);
        Open(&self.open)
    }

    /// Drains the connections: each one sends GOAWAY and finishes its
    /// requests in flight, for up to `grace` (`None` waits for as long as
    /// they take). Whatever is left then is closed.
    pub(crate) async fn drain(&self, grace: Option<Duration>) {
        self.phase.send_replace(Phase::Draining);
        let drained = match grace {
            Some(grace) => tokio::time::timeout(grace, self.all_closed()).await.is_ok(),
            None => {
                self.all_closed().await;
                true
            }
        };
        if !drained {
            eprintln!(
                "{} => Grace period over, closing {} HTTP/3 connections",
                self.context.log.log_name,
                *self.open.borrow()
            );
        }
        self.phase.send_replace(Phase::Closed);
        self.all_closed().await;
    }

    async fn all_closed(&self) {
        // The sender lives in `self`: waiting cannot fail.
        let _ = self.open.subscribe().wait_for(|&open| open == 0).await;
    }
}

/// One connection counted in [`Listener::count_open`] until dropped.
pub(super) struct Open<'a>(&'a watch::Sender<usize>);

impl Drop for Open<'_> {
    fn drop(&mut self) {
        self.0.send_modify(|open| *open -= 1);
    }
}

/// `H3_NO_ERROR` as a QUIC application error code.
pub(super) fn no_error() -> VarInt {
    VarInt::from_u64(Code::H3_NO_ERROR.value()).expect("HTTP/3 error codes are QUIC varints")
}

/// Suite that protects QUIC Initial packets. It is always AES-128-GCM (RFC
/// 9001, section 5.2), even when `tls_options` leave that suite out of the
/// handshake.
fn initial_suite() -> io::Result<Suite> {
    let provider = Arc::clone(ServerConfig::builder().crypto_provider());
    provider
        .cipher_suites
        .iter()
        .filter_map(|suite| suite.tls13())
        .find(|suite| suite.common.suite == CipherSuite::TLS13_AES_128_GCM_SHA256)
        .and_then(|suite| suite.quic_suite())
        .ok_or_else(|| io::Error::other("the TLS provider has no AES-128-GCM suite for QUIC"))
}
