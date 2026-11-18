//! Layer 4 TCP proxy: picks a backend per connection and copies bytes both
//! ways. Optional TLS termination and the choice of the `match` entry happen
//! before this, in the listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rxh_core::config::ProxyProtocol;
use rxh_core::pool::{SelectContext, Upstream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::l4match::L4Matcher;
use crate::log::{self, ListenerLog};
use crate::metrics;
use crate::proxy_protocol;
use crate::registry::Group;
use crate::timeout::{Stamp, Watched};
use crate::upstream::{self, DialError};

/// Time limits of a proxied connection.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Limits {
    /// Close after this long without traffic in either direction.
    pub(crate) idle: Option<Duration>,

    /// Close after this long, whatever the traffic.
    pub(crate) stream: Option<Duration>,
}

/// One `match` entry of a `tcp` server.
#[derive(Debug)]
pub(crate) struct Route {
    /// Layer 4 matchers of the entry.
    pub(crate) matcher: L4Matcher,

    /// Backends of the entry's `forward`.
    pub(crate) group: Arc<Group>,
}

/// Proxies one client connection to a backend of `group`.
pub(crate) async fn proxy<S>(
    client: S,
    peer: SocketAddr,
    local: SocketAddr,
    group: &Group,
    proxy_protocol: Option<ProxyProtocol>,
    limits: Limits,
    log: &ListenerLog,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let log_name = &*log.log_name;
    let started = Instant::now();
    metrics::global().record_l4(&log.server, "tcp");
    let context = SelectContext::l4(peer.ip());
    let dialer = &group.dialer;

    let dialed = upstream::connect_with_retries(
        &group.pool,
        &context,
        |upstream: Arc<Upstream>| async move { dialer.dial(upstream.address()).await },
    )
    .await;

    // Every way a connection can end writes one access record in JSON mode;
    // text mode keeps the operational messages.
    let json = |upstream: Option<&str>, sent: u64, received: u64| {
        log.logger.access_json(|| {
            serde_json::json!({
                "ts": log::now(),
                "server": &*log.server,
                "protocol": "tcp",
                "remote": peer.to_string(),
                "upstream": upstream,
                "bytes_sent": sent,
                "bytes_received": received,
                "duration_ms": log::millis(started.elapsed()),
            })
        });
    };
    let text = log.logger.is_text();

    let (selection, mut backend) = match dialed {
        Ok(dialed) => dialed,
        Err(DialError::NoUpstream) => {
            json(None, 0, 0);
            if text {
                eprintln!("{log_name} => {peer}: no upstreams available");
            }
            return;
        }
        Err(DialError::Connect(err)) => {
            json(None, 0, 0);
            if text {
                eprintln!("{log_name} => {peer}: all dial attempts failed: {err}");
            }
            return;
        }
    };

    let _guard = selection.upstream.start_request();
    let address = selection.upstream.address();

    if let Some(version) = proxy_protocol
        && let Err(err) = backend
            .write_all(&proxy_protocol::header(version, peer, local))
            .await
    {
        selection.upstream.record_failure();
        json(Some(address), 0, 0);
        if text {
            eprintln!("{log_name} => {peer}: PROXY header to {address} failed: {err}");
        }
        return;
    }

    // Traffic from the client is a read of `client`, traffic from the backend
    // a write to it, so watching the client sees both directions.
    let stamp = Stamp::new();
    let mut client = Watched::new(client, Arc::clone(&stamp));
    let copy = tokio::io::copy_bidirectional(&mut client, &mut backend);
    let idle = async {
        match limits.idle {
            Some(idle) => stamp.idle_for(idle, || false).await,
            None => std::future::pending().await,
        }
    };
    let lifetime = async {
        match limits.stream {
            Some(stream) => tokio::time::sleep(stream).await,
            None => std::future::pending().await,
        }
    };

    // ponytail: byte counts are 0 when a timeout or error ends the copy,
    // since the copy future is dropped; wrap the streams in counters if needed.
    tokio::select! {
        result = copy => match result {
            Ok((sent, received)) => {
                json(Some(address), sent, received);
                log.logger.access_text(|| {
                    format!("{peer} -> {log_name} TCP {address} sent {sent}B received {received}B")
                });
            }
            Err(err) => {
                json(Some(address), 0, 0);
                if text {
                    eprintln!("{peer} -> {log_name} TCP {address} closed: {err}");
                }
            }
        },
        () = idle => {
            json(Some(address), 0, 0);
            if text {
                println!("{peer} -> {log_name} TCP {address} closed: idle timeout");
            }
        }
        () = lifetime => {
            json(Some(address), 0, 0);
            if text {
                println!("{peer} -> {log_name} TCP {address} closed: stream timeout");
            }
        }
    }
}
