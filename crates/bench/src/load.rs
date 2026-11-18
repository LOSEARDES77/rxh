//! Closed-loop load generator. Every worker owns one keep-alive connection
//! and sends the next request as soon as the previous response arrives.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{Method, Version, header};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::backend::BACKEND_HEADER;

/// Distinct keys used by the hash based modes.
const KEY_COUNT: usize = 64;

/// Echo round trips before a TCP or WebSocket worker opens a new connection.
const ECHOES_PER_CONNECTION: usize = 50;

/// Workers sharing one HTTP/2 connection, each with one stream in flight.
const STREAMS_PER_CONNECTION: usize = 8;

/// Connections opened at the same time by [`open_idle`].
const OPEN_PARALLELISM: usize = 64;

/// How long a UDP worker waits for an echo before counting a loss.
const UDP_TIMEOUT: Duration = Duration::from_secs(1);

/// TLS name clients send and expect in the certificate.
pub const TLS_NAME: &str = "localhost";

/// What varies between requests, so sticky policies can be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    /// Every request is `GET /`.
    None,

    /// Requests go to `/item/{key}`.
    Path,

    /// Requests carry the given header with value `user-{key}`.
    Header(&'static str),

    /// Each worker is a browser session that stores the `Set-Cookie` it gets.
    Cookie,

    /// Each worker connects from its own loopback address (127.0.0.x).
    SourceIp,
}

/// Where to send load and how much.
#[derive(Debug, Clone, Copy)]
pub struct Load {
    /// Proxy address.
    pub target: SocketAddr,

    /// Parallel connections.
    pub concurrency: usize,

    /// How long to keep sending.
    pub duration: Duration,
}

/// Throughput and latency of a run.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    /// Successful requests (or echo round trips).
    pub requests: u64,

    /// Failed requests: transport errors and non-2xx responses.
    pub errors: u64,

    /// Successful requests per second.
    pub rps: f64,

    /// Median latency in microseconds.
    pub p50_us: u32,

    /// 90th percentile latency in microseconds.
    pub p90_us: u32,

    /// 99th percentile latency in microseconds.
    pub p99_us: u32,

    /// Slowest request in microseconds.
    pub max_us: u32,
}

impl Stats {
    fn new(mut latencies: Vec<u32>, errors: u64, elapsed: Duration) -> Self {
        latencies.sort_unstable();
        let percentile = |quantile: f64| {
            let last = latencies.len().saturating_sub(1) as f64;
            latencies
                .get((last * quantile).round() as usize)
                .copied()
                .unwrap_or_default()
        };

        Self {
            requests: latencies.len() as u64,
            errors,
            rps: latencies.len() as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            p50_us: percentile(0.50),
            p90_us: percentile(0.90),
            p99_us: percentile(0.99),
            max_us: latencies.last().copied().unwrap_or_default(),
        }
    }
}

/// How HTTP workers talk to the proxy.
#[derive(Debug, Clone)]
pub struct Client {
    /// What varies between requests.
    pub keys: KeyMode,

    /// TLS settings. Their ALPN list decides between HTTP/1.1 and HTTP/2;
    /// with `h2`, [`STREAMS_PER_CONNECTION`] workers share a connection.
    pub tls: Option<Arc<ClientConfig>>,

    /// Request body. A non-empty one is `POST`ed and must come back unchanged.
    pub body: Bytes,
}

impl Client {
    /// Cleartext HTTP/1.1 `GET` requests.
    pub fn plain(keys: KeyMode) -> Self {
        Self {
            keys,
            tls: None,
            body: Bytes::new(),
        }
    }
}

/// Result of an HTTP run.
#[derive(Debug, Default)]
pub struct HttpOutcome {
    /// Throughput and latency.
    pub stats: Stats,

    /// Backends seen for every key (see [`KeyMode`]).
    pub key_backends: HashMap<String, BTreeSet<String>>,

    /// Successful responses that came over HTTP/2.
    pub http2: u64,
}

#[derive(Default)]
struct WorkerOutput {
    latencies: Vec<u32>,
    errors: u64,
    key_backends: HashMap<String, BTreeSet<String>>,
    http2: u64,
}

/// A client connection: HTTP/1.1 or a handle to a shared HTTP/2 one.
enum Sender {
    Http1(http1::SendRequest<Full<Bytes>>),
    Http2(http2::SendRequest<Full<Bytes>>),
}

impl Sender {
    async fn ready(&mut self) -> hyper::Result<()> {
        match self {
            Sender::Http1(sender) => sender.ready().await,
            Sender::Http2(sender) => sender.ready().await,
        }
    }

    async fn send(
        &mut self,
        request: Request<Full<Bytes>>,
    ) -> hyper::Result<hyper::Response<Incoming>> {
        match self {
            Sender::Http1(sender) => sender.send_request(request).await,
            Sender::Http2(sender) => sender.send_request(request).await,
        }
    }
}

/// Runs an HTTP load test against `load.target`.
pub async fn http(load: Load, client: &Client) -> HttpOutcome {
    // HTTP/2 workers share connections, dialed up front.
    let mut shared = Vec::new();
    let wants_h2 = client
        .tls
        .as_ref()
        .is_some_and(|tls| tls.alpn_protocols.iter().any(|alpn| alpn == b"h2"));
    if wants_h2 {
        for _ in 0..load.concurrency.div_ceil(STREAMS_PER_CONNECTION) {
            if let Ok(Sender::Http2(sender)) =
                connect(load.target, Ipv4Addr::LOCALHOST, client.tls.as_ref()).await
            {
                shared.push(sender);
            }
        }
    }

    let started = Instant::now();
    let deadline = started + load.duration;
    let mut workers = JoinSet::new();
    for worker in 0..load.concurrency {
        let initial = shared
            .get(worker / STREAMS_PER_CONNECTION)
            .cloned()
            .map(Sender::Http2);
        workers.spawn(http_worker(
            load.target,
            worker,
            client.clone(),
            initial,
            deadline,
        ));
    }

    let mut merged = WorkerOutput::default();
    while let Some(output) = workers.join_next().await {
        let Ok(output) = output else {
            merged.errors += 1;
            continue;
        };
        merged.latencies.extend(output.latencies);
        merged.errors += output.errors;
        merged.http2 += output.http2;
        for (key, backends) in output.key_backends {
            merged.key_backends.entry(key).or_default().extend(backends);
        }
    }

    HttpOutcome {
        stats: Stats::new(merged.latencies, merged.errors, started.elapsed()),
        key_backends: merged.key_backends,
        http2: merged.http2,
    }
}

async fn http_worker(
    target: SocketAddr,
    worker: usize,
    client: Client,
    mut sender: Option<Sender>,
    deadline: Instant,
) -> WorkerOutput {
    let mut output = WorkerOutput::default();
    let keys = client.keys;
    let source = match keys {
        KeyMode::SourceIp => Ipv4Addr::new(127, 0, 0, 2 + (worker % 250) as u8),
        _ => Ipv4Addr::LOCALHOST,
    };
    let host = target.to_string();
    let mut cookie: Option<String> = None;
    let mut sequence = 0;

    while Instant::now() < deadline {
        let mut connection = match sender.take() {
            Some(connection) => connection,
            None => match connect(target, source, client.tls.as_ref()).await {
                Ok(connection) => connection,
                Err(_) => {
                    output.errors += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            },
        };

        // A closed keep-alive connection is not a failed request: reconnect.
        if connection.ready().await.is_err() {
            continue;
        }

        let index = (worker + sequence * 13) % KEY_COUNT;
        sequence += 1;
        let (path, key) = match keys {
            KeyMode::None => (String::from("/"), None),
            KeyMode::Path => (format!("/item/{index}"), Some(format!("/item/{index}"))),
            KeyMode::Header(_) => (String::from("/"), Some(format!("user-{index}"))),
            KeyMode::Cookie => (String::from("/"), Some(format!("session-{worker}"))),
            KeyMode::SourceIp => (String::from("/"), Some(source.to_string())),
        };

        // HTTP/2 carries the host in the `:authority` of an absolute URI.
        let mut builder = match connection {
            Sender::Http1(_) => Request::builder().uri(path).header(header::HOST, &host),
            Sender::Http2(_) => Request::builder().uri(format!("https://{host}{path}")),
        };
        if !client.body.is_empty() {
            builder = builder.method(Method::POST);
        }
        if let (KeyMode::Header(name), Some(value)) = (keys, &key) {
            builder = builder.header(name, value);
        }
        if let Some(cookie) = &cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let request = builder
            .body(Full::new(client.body.clone()))
            .expect("request parts are valid");

        let started = Instant::now();
        let Ok(response) = connection.send(request).await else {
            output.errors += 1;
            continue;
        };

        let status = response.status();
        let version = response.version();
        let backend = response
            .headers()
            .get(BACKEND_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if keys == KeyMode::Cookie
            && let Some(set_cookie) = response
                .headers()
                .get(header::SET_COOKIE)
                .and_then(|value| value.to_str().ok())
        {
            cookie = set_cookie.split(';').next().map(str::to_owned);
        }

        let Ok(body) = response.into_body().collect().await else {
            output.errors += 1;
            continue;
        };
        sender = Some(connection);

        let echoed = client.body.is_empty() || body.to_bytes() == client.body;
        if !status.is_success() || !echoed {
            output.errors += 1;
            continue;
        }

        output.latencies.push(micros(started.elapsed()));
        if version == Version::HTTP_2 {
            output.http2 += 1;
        }
        if let (Some(key), Some(backend)) = (key, backend) {
            output.key_backends.entry(key).or_default().insert(backend);
        }
    }

    output
}

async fn connect(
    target: SocketAddr,
    source: Ipv4Addr,
    tls: Option<&Arc<ClientConfig>>,
) -> io::Result<Sender> {
    // An explicit bind cannot reuse ports in TIME_WAIT, so the thousands of
    // connections of a run would exhaust them: bind only to pick a source IP.
    let stream = if source == Ipv4Addr::LOCALHOST {
        TcpStream::connect(target).await?
    } else {
        let socket = TcpSocket::new_v4()?;
        socket.bind(SocketAddr::from((source, 0)))?;
        socket.connect(target).await?
    };
    stream.set_nodelay(true)?;

    let Some(tls) = tls else {
        return handshake(stream, /* http2= */ false).await;
    };
    let name = ServerName::try_from(TLS_NAME).expect("TLS_NAME is a valid DNS name");
    let stream = TlsConnector::from(Arc::clone(tls))
        .connect(name, stream)
        .await?;
    let http2 = stream.get_ref().1.alpn_protocol() == Some(b"h2");
    handshake(stream, http2).await
}

async fn handshake<T>(io: T, http2: bool) -> io::Result<Sender>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(io);
    if http2 {
        let (sender, connection) = http2::handshake(TokioExecutor::new(), io)
            .await
            .map_err(io::Error::other)?;
        tokio::spawn(connection);
        return Ok(Sender::Http2(sender));
    }
    let (sender, connection) = http1::handshake(io).await.map_err(io::Error::other)?;
    tokio::spawn(connection.with_upgrades());
    Ok(Sender::Http1(sender))
}

/// Idle keep-alive connections opened by [`open_idle`].
pub struct IdleConnections {
    senders: Vec<Sender>,

    /// Latency of the first request on each connection.
    pub stats: Stats,
}

impl IdleConnections {
    /// Connections that got their first response.
    pub fn len(&self) -> usize {
        self.senders.len()
    }

    /// Sends one more `GET /` on every connection and returns how many
    /// failed: closed by the proxy, or answered with an error.
    pub async fn probe(self, target: SocketAddr) -> u64 {
        let mut probes = JoinSet::new();
        let limit = Arc::new(Semaphore::new(OPEN_PARALLELISM));
        for sender in self.senders {
            let limit = Arc::clone(&limit);
            probes.spawn(async move {
                let _permit = limit.acquire().await;
                get(sender, target).await.is_err()
            });
        }
        let mut failed = 0;
        while let Some(result) = probes.join_next().await {
            failed += u64::from(result.unwrap_or(true));
        }
        failed
    }
}

/// Opens `count` HTTP/1.1 keep-alive connections, sends one `GET /` on each
/// and keeps them open, [`OPEN_PARALLELISM`] at a time.
pub async fn open_idle(target: SocketAddr, count: usize) -> IdleConnections {
    let started = Instant::now();
    let limit = Arc::new(Semaphore::new(OPEN_PARALLELISM));
    let mut opens = JoinSet::new();
    for _ in 0..count {
        let limit = Arc::clone(&limit);
        opens.spawn(async move {
            let _permit = limit.acquire().await;
            let sender = connect(target, Ipv4Addr::LOCALHOST, None).await.ok()?;
            let request_started = Instant::now();
            let sender = get(sender, target).await.ok()?;
            Some((sender, micros(request_started.elapsed())))
        });
    }

    let mut senders = Vec::with_capacity(count);
    let mut latencies = Vec::with_capacity(count);
    let mut errors = 0;
    while let Some(result) = opens.join_next().await {
        match result {
            Ok(Some((sender, latency))) => {
                senders.push(sender);
                latencies.push(latency);
            }
            _ => errors += 1,
        }
    }
    IdleConnections {
        senders,
        stats: Stats::new(latencies, errors, started.elapsed()),
    }
}

/// One `GET /` on `sender`, which is handed back when it succeeded.
async fn get(mut sender: Sender, target: SocketAddr) -> Result<Sender, ()> {
    sender.ready().await.map_err(drop)?;
    let request = Request::get("/")
        .header(header::HOST, target.to_string())
        .body(Full::new(Bytes::new()))
        .expect("request parts are valid");
    let response = sender.send(request).await.map_err(drop)?;
    let status = response.status();
    response.into_body().collect().await.map_err(drop)?;
    if status.is_success() {
        Ok(sender)
    } else {
        Err(())
    }
}

/// Runs a TCP echo load test: each round trip writes 64 bytes and reads them
/// back. Workers reconnect every few round trips so the balancer gets to
/// pick backends many times.
pub async fn echo(load: Load) -> Stats {
    let started = Instant::now();
    let deadline = started + load.duration;
    let mut workers = JoinSet::new();
    for _ in 0..load.concurrency {
        workers.spawn(echo_worker(load.target, deadline));
    }
    merge(workers, started).await
}

async fn echo_worker(target: SocketAddr, deadline: Instant) -> WorkerOutput {
    let mut output = WorkerOutput::default();
    let payload = [b'x'; 64];
    let mut buffer = [0; 64];

    while Instant::now() < deadline {
        let Ok(mut stream) = TcpStream::connect(target).await else {
            output.errors += 1;
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };
        let _ = stream.set_nodelay(true);

        for _ in 0..ECHOES_PER_CONNECTION {
            if Instant::now() >= deadline {
                break;
            }
            let started = Instant::now();
            let round_trip = async {
                stream.write_all(&payload).await?;
                stream.read_exact(&mut buffer).await
            };
            if round_trip.await.is_err() || buffer != payload {
                output.errors += 1;
                break;
            }
            output.latencies.push(micros(started.elapsed()));
        }
    }

    output
}

/// Runs a WebSocket load test: each worker upgrades `GET /` and echoes
/// 64 byte binary frames, then closes the WebSocket and opens a new one
/// every few round trips so the balancer picks backends many times.
pub async fn websocket(load: Load) -> Stats {
    let started = Instant::now();
    let deadline = started + load.duration;
    let mut workers = JoinSet::new();
    for _ in 0..load.concurrency {
        workers.spawn(websocket_worker(load.target, deadline));
    }
    merge(workers, started).await
}

async fn websocket_worker(target: SocketAddr, deadline: Instant) -> WorkerOutput {
    let mut output = WorkerOutput::default();
    let payload = [b'w'; 64];
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = vec![0x82, 0x80 | 64];
    frame.extend(mask);
    frame.extend(
        payload
            .iter()
            .zip(mask.iter().cycle())
            .map(|(byte, key)| byte ^ key),
    );
    let mut close = vec![0x88, 0x80];
    close.extend(mask);

    while Instant::now() < deadline {
        let Ok(mut stream) = websocket_upgrade(target).await else {
            output.errors += 1;
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };

        for _ in 0..ECHOES_PER_CONNECTION {
            if Instant::now() >= deadline {
                break;
            }
            let started = Instant::now();
            let mut echoed = [0; 66];
            let round_trip = async {
                stream.write_all(&frame).await?;
                stream.read_exact(&mut echoed).await
            };
            if round_trip.await.is_err() || echoed[..2] != [0x82, 64] || echoed[2..] != payload {
                output.errors += 1;
                break;
            }
            output.latencies.push(micros(started.elapsed()));
        }
        let _ = stream.write_all(&close).await;
    }

    output
}

/// Opens a TCP connection and upgrades it to a WebSocket. Returns the
/// stream positioned after the `101` response.
async fn websocket_upgrade(target: SocketAddr) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(target).await?;
    stream.set_nodelay(true)?;
    let request = format!(
        "GET / HTTP/1.1\r\nHost: {target}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;

    // Byte by byte, so nothing after the head is consumed.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 8192 {
            return Err(io::Error::other("response head too long"));
        }
        head.push(stream.read_u8().await?);
    }
    if !head.starts_with(b"HTTP/1.1 101") {
        return Err(io::Error::other("upgrade refused"));
    }
    Ok(stream)
}

/// Runs a UDP echo load test: each round trip sends 64 bytes and waits for
/// them to come back. Each worker is one long-lived session (one source
/// port), like a game or VPN client; it only moves to a new port after a
/// datagram is not echoed within [`UDP_TIMEOUT`], which counts as an error.
pub async fn udp(load: Load) -> Stats {
    let started = Instant::now();
    let deadline = started + load.duration;
    let mut workers = JoinSet::new();
    for _ in 0..load.concurrency {
        workers.spawn(udp_worker(load.target, deadline));
    }
    merge(workers, started).await
}

async fn udp_worker(target: SocketAddr, deadline: Instant) -> WorkerOutput {
    let mut output = WorkerOutput::default();
    let mut payload = [b'u'; 64];
    let mut buffer = [0; 128];

    while Instant::now() < deadline {
        let socket = match UdpSocket::bind("127.0.0.1:0").await {
            Ok(socket) if socket.connect(target).await.is_ok() => socket,
            _ => {
                output.errors += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
        };

        let mut sequence: u64 = 0;
        while Instant::now() < deadline {
            sequence += 1;
            // The sequence number tells a late echo of a lost round apart.
            payload[..8].copy_from_slice(&sequence.to_be_bytes());
            let started = Instant::now();
            let round_trip = async {
                socket.send(&payload).await?;
                loop {
                    let length = socket.recv(&mut buffer).await?;
                    if buffer[..length] == payload {
                        return io::Result::Ok(());
                    }
                }
            };
            match tokio::time::timeout(UDP_TIMEOUT, round_trip).await {
                Ok(Ok(())) => output.latencies.push(micros(started.elapsed())),
                _ => {
                    output.errors += 1;
                    break;
                }
            }
        }
    }

    output
}

/// Merges the output of echo style workers.
async fn merge(mut workers: JoinSet<WorkerOutput>, started: Instant) -> Stats {
    let mut latencies = Vec::new();
    let mut errors = 0;
    while let Some(output) = workers.join_next().await {
        match output {
            Ok(output) => {
                latencies.extend(output.latencies);
                errors += output.errors;
            }
            Err(_) => errors += 1,
        }
    }
    Stats::new(latencies, errors, started.elapsed())
}

fn micros(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles() {
        let stats = Stats::new((1..=100).collect(), 3, Duration::from_secs(2));
        assert_eq!(stats.requests, 100);
        assert_eq!(stats.errors, 3);
        assert_eq!(stats.p50_us, 51);
        assert_eq!(stats.p99_us, 99);
        assert_eq!(stats.max_us, 100);
        assert!(
            (stats.rps - 50.0).abs() < 1e-9,
            "rps should be 50, got {}",
            stats.rps
        );
        assert_eq!(Stats::new(Vec::new(), 0, Duration::ZERO).p99_us, 0);
    }

    use crate::backend::{HttpBackend, UdpEchoBackend};

    fn quick(target: SocketAddr, concurrency: usize) -> Load {
        Load {
            target,
            concurrency,
            duration: Duration::from_millis(300),
        }
    }

    #[tokio::test]
    async fn bodies_come_back_unchanged() {
        let backend = HttpBackend::spawn(0, Duration::ZERO).await.unwrap();
        let client = Client {
            body: Bytes::from(vec![7; 300_000]),
            ..Client::plain(KeyMode::None)
        };
        let outcome = http(quick(backend.address(), 2), &client).await;
        assert_eq!(outcome.stats.errors, 0);
        assert!(outcome.stats.requests > 0);
        assert_eq!(outcome.http2, 0, "cleartext requests are HTTP/1.1");
    }

    #[tokio::test]
    async fn websocket_frames_echo() {
        let backend = HttpBackend::spawn(0, Duration::ZERO).await.unwrap();
        let stats = websocket(quick(backend.address(), 2)).await;
        assert_eq!(stats.errors, 0);
        assert!(stats.requests > ECHOES_PER_CONNECTION as u64);
        assert!(
            backend.hits() >= 2,
            "every worker upgrades at least once, got {}",
            backend.hits()
        );
    }

    #[tokio::test]
    async fn websocket_refused_upgrade_is_an_error() {
        // A plain TCP echo server answers the upgrade with the request itself.
        let echo = crate::backend::EchoBackend::spawn().await.unwrap();
        let stats = websocket(quick(echo.address(), 1)).await;
        assert_eq!(stats.requests, 0);
        assert!(stats.errors > 0);
    }

    #[tokio::test]
    async fn udp_sessions_last_the_whole_run() {
        let backend = UdpEchoBackend::spawn().await.unwrap();
        let stats = udp(quick(backend.address(), 3)).await;
        assert_eq!(stats.errors, 0);
        assert!(stats.requests > 3);
        assert_eq!(backend.sessions(), 3, "one session per worker");
    }

    #[tokio::test]
    async fn udp_without_echo_counts_losses() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stats = udp(quick(silent.local_addr().unwrap(), 1)).await;
        assert_eq!(stats.requests, 0);
        assert_eq!(stats.errors, 1, "one lost datagram per timeout");
    }

    #[tokio::test]
    async fn idle_connections_stay_usable_until_the_server_goes() {
        let backend = HttpBackend::spawn(0, Duration::ZERO).await.unwrap();
        let idle = open_idle(backend.address(), 150).await;
        assert_eq!(idle.len(), 150);
        assert_eq!(idle.stats.requests, 150);
        assert_eq!(idle.probe(backend.address()).await, 0);

        let idle = open_idle(backend.address(), 20).await;
        backend.stop();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            idle.probe(backend.address()).await,
            20,
            "a stopped backend closes every connection"
        );
    }
}
