//! Helpers for integration tests: throwaway backends, a tiny HTTP client and
//! proxies started from TOML snippets on port 0.

#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rxh_server::config::Config;
use rxh_server::{Master, Registry, Server, State};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch};

/// Parses and validates a configuration.
pub fn config(text: &str) -> Config {
    Config::from_toml(text).unwrap_or_else(|err| panic!("invalid test config: {err}\n{text}"))
}

/// Starts every server of `text` and returns their addresses in order.
pub fn spawn(text: &str) -> Vec<SocketAddr> {
    spawn_with_registry(text).0
}

/// Same as [`spawn`], also returning the pools.
pub fn spawn_with_registry(text: &str) -> (Vec<SocketAddr>, Arc<Registry>) {
    let master = Master::from_config(&config(text)).expect("master starts");
    let sockets = master.sockets();
    let registry = Arc::clone(master.registry());
    tokio::task::spawn(master.run());
    (sockets, registry)
}

/// Starts the first server of `text` with a shutdown trigger and its state.
pub fn spawn_controlled(text: &str) -> (SocketAddr, impl FnOnce() + use<>, watch::Receiver<State>) {
    let config = config(text);
    let registry = Registry::build(&config).expect("pools build");
    let (trigger, shutdown) = oneshot::channel::<()>();
    let server = Server::init(&config, &registry, config.servers[0].listen[0], &[0])
        .expect("server binds")
        .shutdown_on(shutdown);
    let address = server.socket_address();
    let state = server.subscribe();
    tokio::task::spawn(server.run());
    (
        address,
        move || trigger.send(()).expect("server is running"),
        state,
    )
}

/// Address that refuses connections.
pub fn dead_address() -> SocketAddr {
    let socket = tokio::net::TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind");
    let address = socket.local_addr().expect("address");
    // Bound but never listening: connections are refused, and the port stays taken for the whole
    // test binary, so a listener of a concurrent test cannot be given the same port.
    std::mem::forget(socket);
    address
}

async fn serve_http<F, Fut>(listener: TcpListener, handler: F)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let handler = handler.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |request| {
                let handler = handler.clone();
                async move { Ok::<_, Infallible>(handler(request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
    }
}

/// Backend answering each request with `handler`.
pub async fn backend_with<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<Full<Bytes>>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(serve_http(listener, handler));
    address
}

/// Backend answering every request with `body`.
pub async fn backend(body: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(serve_http(listener, move |_| async move {
        Response::new(Full::from(body))
    }));
    address
}

/// Request and connection counters of a backend.
#[derive(Clone, Default)]
pub struct Counters {
    pub requests: Arc<AtomicUsize>,
    pub connections: Arc<AtomicUsize>,
}

impl Counters {
    pub fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Backend that counts requests and accepted connections, answering with
/// its own address. Requests to `/health` return `health` and are not counted.
pub async fn counting_backend(health: Arc<AtomicU16>) -> (SocketAddr, Counters) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let counters = Counters::default();
    let task_counters = counters.clone();

    tokio::task::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            task_counters.connections.fetch_add(1, Ordering::SeqCst);
            let counters = task_counters.clone();
            let health = Arc::clone(&health);
            tokio::task::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let counters = counters.clone();
                    let health = Arc::clone(&health);
                    async move {
                        if request.uri().path() == "/health" {
                            let status = StatusCode::from_u16(health.load(Ordering::SeqCst))
                                .expect("valid status");
                            let mut response = Response::new(Full::<Bytes>::from("health"));
                            *response.status_mut() = status;
                            return Ok::<_, Infallible>(response);
                        }
                        counters.requests.fetch_add(1, Ordering::SeqCst);
                        Ok(Response::new(Full::from(address.to_string())))
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    (address, counters)
}

/// Healthy counting backends.
pub async fn counting_backends(count: usize) -> Vec<(SocketAddr, Counters)> {
    let mut backends = Vec::new();
    for _ in 0..count {
        backends.push(counting_backend(Arc::new(AtomicU16::new(200))).await);
    }
    backends
}

/// Backend that forwards the request head it receives to the returned channel.
pub async fn interceptor() -> (SocketAddr, mpsc::Receiver<http::request::Parts>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = mpsc::channel(16);
    tokio::task::spawn(serve_http(listener, move |request: Request<Incoming>| {
        let sender = sender.clone();
        async move {
            let (parts, _) = request.into_parts();
            let _ = sender.send(parts).await;
            let mut response = Response::new(Full::from("intercepted"));
            response
                .headers_mut()
                .insert("x-response-remove", "upstream".parse().expect("valid"));
            response
        }
    }));
    (address, receiver)
}

/// Backend that forwards each request head and body to the returned channel
/// and answers `status`.
pub async fn body_interceptor(
    status: u16,
) -> (SocketAddr, mpsc::Receiver<(http::request::Parts, Bytes)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = mpsc::channel(16);
    tokio::task::spawn(serve_http(listener, move |request: Request<Incoming>| {
        let sender = sender.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = body.collect().await.expect("body").to_bytes();
            let _ = sender.send((parts, body)).await;
            let mut response = Response::new(Full::default());
            *response.status_mut() = StatusCode::from_u16(status).expect("valid status");
            response
        }
    }));
    (address, receiver)
}

/// Backend answering with the request body, and with the `Content-Length` it
/// received (or `none`) in `x-request-length`. Also returns the number of
/// requests whose head arrived.
pub async fn echo_backend() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::task::spawn(serve_http(listener, move |request: Request<Incoming>| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move {
            let length = request
                .headers()
                .get("content-length")
                .map_or("none", |value| value.to_str().expect("ascii"))
                .to_owned();
            let body = request.into_body().collect().await;
            let body = body.map(|body| body.to_bytes()).unwrap_or_default();
            let mut response = Response::new(Full::from(body));
            response
                .headers_mut()
                .insert("x-request-length", length.parse().expect("valid"));
            response
        }
    }));
    (address, requests)
}

/// Backend answering every request with `status`.
pub async fn status_backend(status: u16) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(serve_http(listener, move |_| async move {
        let mut response = Response::new(Full::from("status"));
        *response.status_mut() = StatusCode::from_u16(status).expect("valid status");
        response
    }));
    address
}

/// TCP server that echoes everything, prefixed by `tag`.
pub async fn tcp_echo(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::task::spawn(async move {
                let mut buffer = [0; 4096];
                while let Ok(read) = stream.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    let mut reply = tag.as_bytes().to_vec();
                    reply.extend_from_slice(&buffer[..read]);
                    if stream.write_all(&reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    address
}

/// TCP server that reports the first `len` bytes of each connection.
pub async fn tcp_capture(len: usize) -> (SocketAddr, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let (sender, receiver) = mpsc::channel(4);
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = vec![0; len];
            if stream.read_exact(&mut buffer).await.is_ok() {
                let _ = sender.send(buffer).await;
            }
        }
    });
    (address, receiver)
}

/// UDP server that echoes datagrams, prefixed by `tag`.
pub async fn udp_echo(tag: &'static str) -> SocketAddr {
    let socket = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
    let address = socket.local_addr().expect("address");
    tokio::task::spawn(async move {
        let mut buffer = [0; 2048];
        while let Ok((read, peer)) = socket.recv_from(&mut buffer).await {
            let mut reply = tag.as_bytes().to_vec();
            reply.extend_from_slice(&buffer[..read]);
            let _ = socket.send_to(&reply, peer).await;
        }
    });
    address
}

/// Sends `request` on a new connection and returns status, headers and body.
pub async fn send(
    address: SocketAddr,
    request: Request<Empty<Bytes>>,
) -> (StatusCode, HeaderMap, Bytes) {
    let stream = TcpStream::connect(address)
        .await
        .expect("proxy accepts connections");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let response = sender.send_request(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    (parts.status, parts.headers, body)
}

/// `GET uri` on a new connection.
pub async fn get(address: SocketAddr, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    send(
        address,
        Request::get(uri).body(Empty::new()).expect("valid request"),
    )
    .await
}

/// Polls `condition` every 20ms for up to 5s.
pub async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    for _ in 0..250 {
        if condition().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met within 5s");
}
