//! Closed-loop load generator. Every worker owns one keep-alive connection
//! and sends the next request as soon as the previous response arrives.

use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::client::conn::http1::SendRequest;
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::task::JoinSet;

use crate::backend::BACKEND_HEADER;

/// Distinct keys used by the hash based modes.
const KEY_COUNT: usize = 64;

/// Echo round trips before a TCP worker opens a new connection.
const ECHOES_PER_CONNECTION: usize = 50;

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

/// Result of an HTTP run.
#[derive(Debug, Default)]
pub struct HttpOutcome {
    /// Throughput and latency.
    pub stats: Stats,

    /// Backends seen for every key (see [`KeyMode`]).
    pub key_backends: HashMap<String, BTreeSet<String>>,
}

#[derive(Default)]
struct WorkerOutput {
    latencies: Vec<u32>,
    errors: u64,
    key_backends: HashMap<String, BTreeSet<String>>,
}

/// Runs an HTTP load test against `load.target`.
pub async fn http(load: Load, keys: KeyMode) -> HttpOutcome {
    let started = Instant::now();
    let deadline = started + load.duration;
    let mut workers = JoinSet::new();
    for worker in 0..load.concurrency {
        workers.spawn(http_worker(load.target, worker, keys, deadline));
    }

    let mut merged = WorkerOutput::default();
    while let Some(output) = workers.join_next().await {
        let Ok(output) = output else {
            merged.errors += 1;
            continue;
        };
        merged.latencies.extend(output.latencies);
        merged.errors += output.errors;
        for (key, backends) in output.key_backends {
            merged.key_backends.entry(key).or_default().extend(backends);
        }
    }

    HttpOutcome {
        stats: Stats::new(merged.latencies, merged.errors, started.elapsed()),
        key_backends: merged.key_backends,
    }
}

async fn http_worker(
    target: SocketAddr,
    worker: usize,
    keys: KeyMode,
    deadline: Instant,
) -> WorkerOutput {
    let mut output = WorkerOutput::default();
    let source = match keys {
        KeyMode::SourceIp => Ipv4Addr::new(127, 0, 0, 2 + (worker % 250) as u8),
        _ => Ipv4Addr::LOCALHOST,
    };
    let host = target.to_string();
    let mut sender: Option<SendRequest<Empty<Bytes>>> = None;
    let mut cookie: Option<String> = None;
    let mut sequence = 0;

    while Instant::now() < deadline {
        let mut client = match sender.take() {
            Some(client) => client,
            None => match connect(target, source).await {
                Ok(client) => client,
                Err(_) => {
                    output.errors += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                }
            },
        };

        // A closed keep-alive connection is not a failed request: reconnect.
        if client.ready().await.is_err() {
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

        let mut builder = Request::get(path).header(header::HOST, &host);
        if let (KeyMode::Header(name), Some(value)) = (keys, &key) {
            builder = builder.header(name, value);
        }
        if let Some(cookie) = &cookie {
            builder = builder.header(header::COOKIE, cookie);
        }
        let request = builder.body(Empty::new()).expect("request parts are valid");

        let started = Instant::now();
        let Ok(response) = client.send_request(request).await else {
            output.errors += 1;
            continue;
        };

        let status = response.status();
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

        if response.into_body().collect().await.is_err() {
            output.errors += 1;
            continue;
        }
        sender = Some(client);

        if !status.is_success() {
            output.errors += 1;
            continue;
        }

        output.latencies.push(micros(started.elapsed()));
        if let (Some(key), Some(backend)) = (key, backend) {
            output.key_backends.entry(key).or_default().insert(backend);
        }
    }

    output
}

async fn connect(target: SocketAddr, source: Ipv4Addr) -> io::Result<SendRequest<Empty<Bytes>>> {
    let socket = TcpSocket::new_v4()?;
    socket.bind(SocketAddr::from((source, 0)))?;
    let stream = socket.connect(target).await?;
    stream.set_nodelay(true)?;

    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
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
}
