//! Layer 4 TCP proxy: picks a backend per connection and copies bytes both
//! ways. Optional TLS termination and the choice of the `match` entry happen
//! before this, in the listener.
//!
//! On Linux, when neither side is encrypted by rxh and the backend is a TCP
//! socket, the bytes move with `splice` and never leave the kernel. Every
//! other case (TLS on either side, unix socket backends, other systems)
//! copies through a user space buffer.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rxh_core::config::ProxyProtocol;
use rxh_core::pool::{SelectContext, Upstream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::server::TlsStream;

use crate::l4match::{L4Matcher, Rewind};
use crate::log::{self, ListenerLog};
use crate::metrics;
use crate::proxy_protocol;
use crate::registry::{Group, Split};
#[cfg(target_os = "linux")]
use crate::splice;
use crate::stream::UpstreamStream;
use crate::timeout::{Stamp, Timed, Watched};
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
    pub(crate) split: Split,
}

/// A client connection, which says whether its bytes can stay in the kernel.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) trait Client: AsyncRead + AsyncWrite + Unpin {
    /// The TCP socket whose bytes are the bytes of this stream, or `None`
    /// when the stream transforms them (TLS).
    fn socket(&self) -> Option<&TcpStream>;

    /// True while reads return bytes read ahead instead of reading the
    /// socket.
    fn has_replay(&self) -> bool;
}

impl Client for TcpStream {
    fn socket(&self) -> Option<&TcpStream> {
        Some(self)
    }

    fn has_replay(&self) -> bool {
        false
    }
}

impl<S: Client> Client for Rewind<S> {
    fn socket(&self) -> Option<&TcpStream> {
        self.get_ref().socket()
    }

    fn has_replay(&self) -> bool {
        self.has_prefix() || self.get_ref().has_replay()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client for TlsStream<S> {
    fn socket(&self) -> Option<&TcpStream> {
        None
    }

    fn has_replay(&self) -> bool {
        false
    }
}

/// Copies both ways until both sides reach end of file. Returns the bytes
/// read from `client` and the bytes written to it; `stamp` is the one
/// `client` records its traffic in.
async fn relay<S: Client>(
    client: &mut Watched<S>,
    backend: &mut Timed<UpstreamStream>,
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] stamp: &Stamp,
) -> io::Result<(u64, u64)> {
    #[cfg(target_os = "linux")]
    if client.get_ref().socket().is_some() && matches!(backend.get_ref(), UpstreamStream::Tcp(_)) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Bytes read while sniffing or parsing a PROXY header are in user
        // space already.
        let mut buffer = [0; 4096];
        while client.get_ref().has_replay() {
            let read = client.read(&mut buffer).await?;
            backend.write_all(&buffer[..read]).await?;
        }

        let (read_limit, write_limit) = backend.limits();
        let (Some(client), UpstreamStream::Tcp(backend)) =
            (client.get_ref().socket(), backend.get_ref())
        else {
            unreachable!("both sides were plain TCP sockets before the replay");
        };
        tokio::try_join!(
            splice::copy(client, backend, None, write_limit, |sent| stamp
                .record_read(sent)),
            splice::copy(backend, client, read_limit, None, |received| {
                stamp.record_written(received);
            }),
        )?;
        return Ok((stamp.read(), stamp.written()));
    }

    tokio::io::copy_bidirectional(client, backend).await
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
    S: Client,
{
    let log_name = &*log.log_name;
    let started = Instant::now();
    metrics::global().record_l4(&log.server, "tcp");
    let context = SelectContext::l4(peer.ip());
    let dialer = &group.dialer;
    let preface: Vec<u8> = proxy_protocol
        .map(|version| proxy_protocol::header(version, peer, local))
        .unwrap_or_default();
    let preface = &preface;

    let dialed = upstream::connect_with_retries(
        &group.pool,
        &context,
        |upstream: Arc<Upstream>| async move {
            dialer.dial_with_preface(upstream.address(), preface).await
        },
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

    // Traffic from the client is a read of `client`, traffic from the backend
    // a write to it, so watching the client sees both directions.
    let stamp = Stamp::new();
    let mut client = Watched::new(client, Arc::clone(&stamp));
    let copy = relay(&mut client, &mut backend, &stamp);
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

    // Reads of `client` were sent upstream, writes to it were received; the
    // stamp keeps the counts when a timeout or an error drops the copy.
    let moved = || (stamp.read(), stamp.written());
    tokio::select! {
        result = copy => match result {
            Ok((sent, received)) => {
                json(Some(address), sent, received);
                log.logger.access_text(|line| {
                    log::push(
                        line,
                        format_args!(
                            "{peer} -> {log_name} TCP {address} sent {sent}B received {received}B"
                        ),
                    );
                });
            }
            Err(err) => {
                let (sent, received) = moved();
                json(Some(address), sent, received);
                if text {
                    eprintln!("{peer} -> {log_name} TCP {address} closed: {err}");
                }
            }
        },
        () = idle => {
            let (sent, received) = moved();
            json(Some(address), sent, received);
            if text {
                println!("{peer} -> {log_name} TCP {address} closed: idle timeout");
            }
        }
        () = lifetime => {
            let (sent, received) = moved();
            json(Some(address), sent, received);
            if text {
                println!("{peer} -> {log_name} TCP {address} closed: stream timeout");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn rewound_socket_is_spliceable_once_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let mut client = Rewind::new(b"ab".to_vec(), Rewind::new(b"c".to_vec(), socket));

        assert!(client.socket().is_some());
        let mut replayed = [0; 3];
        for expected in [b"ab".as_slice(), b"c"] {
            assert!(client.has_replay());
            let read = client.read(&mut replayed).await.unwrap();
            assert_eq!(&replayed[..read], expected);
        }
        assert!(!client.has_replay());
    }
}
