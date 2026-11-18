//! Backends used as load balancing targets: HTTP servers that count hits and
//! TCP echo servers that count connections.

use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Response header that carries the id of the backend that answered.
pub const BACKEND_HEADER: &str = "x-backend";

/// State shared by every connection of one [`HttpBackend`].
struct Shared {
    id: usize,
    delay: Duration,
    hits: AtomicU64,
    healthy: AtomicBool,
}

/// HTTP/1.1 backend. It counts requests (except `/health`), can be slowed
/// down, can report itself unhealthy on `/health` and can be stopped.
pub struct HttpBackend {
    address: SocketAddr,
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
}

impl HttpBackend {
    /// Starts a backend on a random loopback port. Every request waits
    /// `delay` before the response is sent.
    pub async fn spawn(id: usize, delay: Duration) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let shared = Arc::new(Shared {
            id,
            delay,
            hits: AtomicU64::new(0),
            healthy: AtomicBool::new(true),
        });
        let (stop, stopped) = watch::channel(false);
        tokio::spawn(accept_http(listener, Arc::clone(&shared), stopped));
        Ok(Self {
            address,
            shared,
            stop,
        })
    }

    /// Listening address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Requests served, health checks excluded.
    pub fn hits(&self) -> u64 {
        self.shared.hits.load(Ordering::Relaxed)
    }

    /// Makes `/health` answer 200 (`true`) or 503 (`false`). Normal requests
    /// keep working either way.
    pub fn set_healthy(&self, healthy: bool) {
        self.shared.healthy.store(healthy, Ordering::Relaxed);
    }

    /// Closes the listener and every open connection, like a crashed process.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }
}

async fn accept_http(
    listener: TcpListener,
    shared: Arc<Shared>,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        let stream = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => stream,
                Err(_) => continue,
            },
            // Also fires when the `HttpBackend` is dropped.
            _ = stopped.changed() => return,
        };

        let shared = Arc::clone(&shared);
        let mut stopped = stopped.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| respond(Arc::clone(&shared), request));
            let connection = http1::Builder::new().serve_connection(TokioIo::new(stream), service);
            tokio::select! {
                _ = connection => {}
                _ = stopped.changed() => {}
            }
        });
    }
}

async fn respond(
    shared: Arc<Shared>,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if request.uri().path() == "/health" {
        let mut response = Response::new(Full::new(Bytes::from_static(b"health")));
        if !shared.healthy.load(Ordering::Relaxed) {
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        }
        return Ok(response);
    }

    shared.hits.fetch_add(1, Ordering::Relaxed);
    if !shared.delay.is_zero() {
        tokio::time::sleep(shared.delay).await;
    }

    let mut response = Response::new(Full::new(Bytes::from(format!("backend-{}", shared.id))));
    response
        .headers_mut()
        .insert(BACKEND_HEADER, HeaderValue::from(shared.id));
    Ok(response)
}

/// TCP backend that echoes every byte back and counts connections.
pub struct EchoBackend {
    address: SocketAddr,
    connections: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl EchoBackend {
    /// Starts an echo server on a random loopback port.
    pub async fn spawn() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let connections = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&connections);

        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::Relaxed);
                tokio::spawn(async move {
                    let (mut reader, mut writer) = stream.split();
                    let _ = tokio::io::copy(&mut reader, &mut writer).await;
                });
            }
        });

        Ok(Self {
            address,
            connections,
            task,
        })
    }

    /// Listening address.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Connections accepted so far.
    pub fn connections(&self) -> u64 {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for EchoBackend {
    fn drop(&mut self) {
        self.task.abort();
    }
}
