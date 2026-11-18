//! Benchmark scenarios. Each one starts fresh backends and a fresh proxy,
//! sends load, then checks that the balancer behaved as its policy promises.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use clap::ValueEnum;
use serde::Serialize;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::backend::{EchoBackend, HttpBackend, UdpEchoBackend};
use crate::load;
use crate::load::{Client, KeyMode, Load, Stats};
use crate::target::{Problem, Protocol, ProxySpec, Target, TlsFiles};

/// Request body size of the `large-body` scenario.
const LARGE_BODY: usize = 1024 * 1024;

/// Connections opened before the memory baseline, so allocator and pool
/// warm-up is not charged to the measured connections.
const MEMORY_WARMUP: usize = 100;

/// A benchmark scenario.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    /// round_robin over 4 backends: raw throughput and even spread.
    Throughput,

    /// weighted_round_robin with weights 1, 3, 2: exact proportions.
    Weighted,

    /// least_conn with one backend 50ms slower: the slow one gets little traffic.
    LeastConn,

    /// ip_hash: each client IP sticks to one backend.
    IpHash,

    /// uri_hash: each path sticks to one backend.
    UriHash,

    /// header policy on `x-user`: each header value sticks to one backend.
    Header,

    /// cookie policy: each browser session sticks to one backend.
    Cookie,

    /// Passive health + retries: a backend dies mid-run, clients see no errors.
    Failover,

    /// Active health checks: a backend reports 503 on /health and stops
    /// receiving traffic.
    ActiveHealth,

    /// Layer 4 TCP echo through round_robin.
    Tcp,

    /// HTTPS termination: HTTP/1.1 over TLS to 2 backends.
    Tls,

    /// HTTP/2 clients over TLS (ALPN h2), 8 streams per connection.
    Http2,

    /// 1 MiB request bodies, echoed back by the backends.
    LargeBody,

    /// WebSocket upgrades, then 64 byte frames echoed by the backends.
    Websocket,

    /// UDP echo through round_robin, one long-lived session per client.
    Udp,

    /// Load while thousands of idle keep-alive connections are open; the idle
    /// ones must still work afterwards.
    Idle,

    /// Proxy memory (RSS) per idle keep-alive connection.
    Memory,
}

impl Scenario {
    /// Name used on the command line and in reports.
    pub fn name(self) -> String {
        self.to_possible_value()
            .map(|value| value.get_name().to_owned())
            .unwrap_or_default()
    }
}

/// Correctness result of one scenario on one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Behaved as the policy promises.
    Pass,

    /// Misbehaved or broke.
    Fail,

    /// The target cannot run this scenario.
    #[serde(rename = "n/a")]
    NotAvailable,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::NotAvailable => "n/a",
        })
    }
}

/// Everything measured for one scenario on one target.
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// Scenario.
    pub scenario: Scenario,

    /// Target name.
    pub target: String,

    /// Correctness result.
    pub verdict: Verdict,

    /// Why the verdict was given.
    pub reason: String,

    /// Throughput and latency, when load ran.
    pub stats: Option<Stats>,

    /// Requests (or TCP connections) received by each backend.
    pub distribution: Vec<u64>,
}

/// Knobs shared by every scenario.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    /// Load duration.
    pub duration: Duration,

    /// Parallel client connections.
    pub concurrency: usize,

    /// Idle connections held open by the `idle` and `memory` scenarios.
    pub idle_connections: usize,
}

/// Measurement before it is labelled with scenario and target.
struct Measured {
    pass: bool,
    reason: String,
    stats: Stats,
    distribution: Vec<u64>,
}

/// Runs `scenario` against `target`.
pub async fn run(scenario: Scenario, target: &Target, options: Options) -> Outcome {
    let result = match scenario {
        Scenario::Throughput => throughput(target, options).await,
        Scenario::Weighted => weighted(target, options).await,
        Scenario::LeastConn => least_conn(target, options).await,
        Scenario::IpHash => sticky(target, options, "ip_hash", None, KeyMode::SourceIp).await,
        Scenario::UriHash => sticky(target, options, "uri_hash", None, KeyMode::Path).await,
        Scenario::Header => {
            sticky(
                target,
                options,
                "header",
                Some("x-user"),
                KeyMode::Header("x-user"),
            )
            .await
        }
        Scenario::Cookie => sticky(target, options, "cookie", None, KeyMode::Cookie).await,
        Scenario::Failover => failover(target, options).await,
        Scenario::ActiveHealth => active_health(target, options).await,
        Scenario::Tcp => tcp(target, options).await,
        Scenario::Tls => tls(target, options, /* http2= */ false).await,
        Scenario::Http2 => tls(target, options, /* http2= */ true).await,
        Scenario::LargeBody => large_body(target, options).await,
        Scenario::Websocket => websocket(target, options).await,
        Scenario::Udp => udp(target, options).await,
        Scenario::Idle => idle(target, options).await,
        Scenario::Memory => memory(target, options).await,
    };

    let (verdict, reason, stats, distribution) = match result {
        Ok(measured) => (
            if measured.pass {
                Verdict::Pass
            } else {
                Verdict::Fail
            },
            measured.reason,
            Some(measured.stats),
            measured.distribution,
        ),
        Err(Problem::Unsupported(reason)) => (Verdict::NotAvailable, reason, None, Vec::new()),
        Err(Problem::Failed(reason)) => (Verdict::Fail, reason, None, Vec::new()),
    };

    Outcome {
        scenario,
        target: target.name.to_owned(),
        verdict,
        reason,
        stats,
        distribution,
    }
}

async fn spawn_backends(delays: &[Duration]) -> Result<Vec<HttpBackend>, Problem> {
    let mut backends = Vec::with_capacity(delays.len());
    for (id, delay) in delays.iter().enumerate() {
        let backend = HttpBackend::spawn(id, *delay)
            .await
            .map_err(|err| Problem::Failed(format!("backend: {err}")))?;
        backends.push(backend);
    }
    Ok(backends)
}

fn hits(backends: &[HttpBackend]) -> Vec<u64> {
    backends.iter().map(HttpBackend::hits).collect()
}

fn shares(counts: &[u64]) -> Vec<f64> {
    let total = counts.iter().sum::<u64>().max(1) as f64;
    counts.iter().map(|count| *count as f64 / total).collect()
}

fn load(target: std::net::SocketAddr, options: Options) -> Load {
    Load {
        target,
        concurrency: options.concurrency,
        duration: options.duration,
    }
}

fn addresses(backends: &[HttpBackend]) -> Vec<std::net::SocketAddr> {
    backends.iter().map(HttpBackend::address).collect()
}

/// Failed attempts as a percentage of all attempts.
fn error_rate(stats: &Stats) -> f64 {
    let attempts = stats.requests + stats.errors;
    stats.errors as f64 * 100.0 / attempts.max(1) as f64
}

/// Largest distance, in percentage points, between observed and expected shares.
fn max_deviation(counts: &[u64], expected: &[f64]) -> f64 {
    let observed = shares(counts);
    observed
        .iter()
        .zip(expected)
        .map(|(seen, want)| (seen - want).abs() * 100.0)
        .fold(0.0, f64::max)
}

async fn throughput(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 4]).await?;
    let proxy = target
        .start(&ProxySpec::http("round_robin", addresses(&backends)))
        .await?;
    let outcome = load::http(
        load(proxy.address(), options),
        &Client::plain(KeyMode::None),
    )
    .await;

    let distribution = hits(&backends);
    let deviation = max_deviation(&distribution, &[0.25; 4]);
    Ok(Measured {
        pass: outcome.stats.errors == 0 && outcome.stats.requests > 0 && deviation <= 5.0,
        reason: format!(
            "spread off by {deviation:.1}pp max, {} errors",
            outcome.stats.errors
        ),
        stats: outcome.stats,
        distribution,
    })
}

async fn weighted(target: &Target, options: Options) -> Result<Measured, Problem> {
    let weights = [1, 3, 2];
    let backends = spawn_backends(&[Duration::ZERO; 3]).await?;
    let mut spec = ProxySpec::http("weighted_round_robin", addresses(&backends));
    for ((_, weight), wanted) in spec.backends.iter_mut().zip(weights) {
        *weight = wanted;
    }
    let proxy = target.start(&spec).await?;
    let outcome = load::http(
        load(proxy.address(), options),
        &Client::plain(KeyMode::None),
    )
    .await;

    let distribution = hits(&backends);
    let total: u32 = weights.iter().sum();
    let expected: Vec<f64> = weights
        .iter()
        .map(|weight| f64::from(*weight) / f64::from(total))
        .collect();
    let deviation = max_deviation(&distribution, &expected);
    Ok(Measured {
        pass: outcome.stats.errors == 0 && outcome.stats.requests > 0 && deviation <= 3.0,
        reason: format!("weights 1:3:2, off by {deviation:.1}pp max"),
        stats: outcome.stats,
        distribution,
    })
}

async fn least_conn(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends =
        spawn_backends(&[Duration::from_millis(50), Duration::ZERO, Duration::ZERO]).await?;
    let proxy = target
        .start(&ProxySpec::http("least_conn", addresses(&backends)))
        .await?;
    let options = Options {
        concurrency: options.concurrency.max(8),
        ..options
    };
    let outcome = load::http(
        load(proxy.address(), options),
        &Client::plain(KeyMode::None),
    )
    .await;

    let distribution = hits(&backends);
    let slow_share = shares(&distribution)[0] * 100.0;
    Ok(Measured {
        pass: outcome.stats.errors == 0 && outcome.stats.requests > 0 && slow_share < 20.0,
        reason: format!("slow backend got {slow_share:.1}% (round robin would give 33%)"),
        stats: outcome.stats,
        distribution,
    })
}

async fn sticky(
    target: &Target,
    options: Options,
    policy: &'static str,
    key: Option<&'static str>,
    keys: KeyMode,
) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 3]).await?;
    let mut spec = ProxySpec::http(policy, addresses(&backends));
    spec.key = key;
    let proxy = target.start(&spec).await?;
    let outcome = load::http(load(proxy.address(), options), &Client::plain(keys)).await;

    let moved = outcome
        .key_backends
        .values()
        .filter(|seen| seen.len() > 1)
        .count();
    let used: BTreeSet<&String> = outcome.key_backends.values().flatten().collect();
    Ok(Measured {
        pass: outcome.stats.errors == 0 && moved == 0 && used.len() >= 2,
        reason: format!(
            "{} keys, {moved} hit more than one backend, {} backends used",
            outcome.key_backends.len(),
            used.len()
        ),
        stats: outcome.stats,
        distribution: hits(&backends),
    })
}

async fn failover(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 3]).await?;
    let mut spec = ProxySpec::http("round_robin", addresses(&backends));
    spec.retries = 2;
    spec.passive = true;
    let proxy = target.start(&spec).await?;

    let kill_after = options.duration / 3;
    let client = Client::plain(KeyMode::None);
    let (outcome, ()) = tokio::join!(load::http(load(proxy.address(), options), &client), async {
        tokio::time::sleep(kill_after).await;
        backends[0].stop();
    });

    let stats = outcome.stats;
    let error_rate = error_rate(&stats);
    Ok(Measured {
        pass: stats.requests > 0 && error_rate <= 1.0,
        reason: format!(
            "backend 0 killed after {:.1}s: {} client errors ({error_rate:.2}%)",
            kill_after.as_secs_f64(),
            stats.errors
        ),
        stats,
        distribution: hits(&backends),
    })
}

async fn active_health(target: &Target, options: Options) -> Result<Measured, Problem> {
    const GRACE: Duration = Duration::from_secs(3);

    let backends = spawn_backends(&[Duration::ZERO; 3]).await?;
    let mut spec = ProxySpec::http("round_robin", addresses(&backends));
    spec.health_uri = Some("/health");
    let proxy = target.start(&spec).await?;

    let duration = options.duration.max(Duration::from_secs(7));
    let flip_after = (duration / 3).max(Duration::from_secs(2));
    let options = Options {
        duration,
        ..options
    };
    let client = Client::plain(KeyMode::None);
    let (outcome, hits_after_grace) =
        tokio::join!(load::http(load(proxy.address(), options), &client), async {
            tokio::time::sleep(flip_after).await;
            backends[0].set_healthy(false);
            tokio::time::sleep(GRACE).await;
            backends[0].hits()
        });

    let late = backends[0].hits() - hits_after_grace;
    // Requests already in flight when the check flipped may still land.
    let allowed = options.concurrency as u64;
    Ok(Measured {
        pass: error_rate(&outcome.stats) <= 1.0 && hits_after_grace > 0 && late <= allowed,
        reason: format!(
            "unhealthy backend got {late} requests more than {GRACE:?} after /health=503, \
             {} client errors",
            outcome.stats.errors
        ),
        stats: outcome.stats,
        distribution: hits(&backends),
    })
}

async fn tcp(target: &Target, options: Options) -> Result<Measured, Problem> {
    let mut backends = Vec::new();
    for _ in 0..2 {
        backends.push(
            EchoBackend::spawn()
                .await
                .map_err(|err| Problem::Failed(format!("echo: {err}")))?,
        );
    }
    let mut spec = ProxySpec::http("round_robin", backends.iter().map(EchoBackend::address));
    spec.protocol = Protocol::Tcp;
    let proxy = target.start(&spec).await?;
    let stats = load::echo(load(proxy.address(), options)).await;

    let distribution: Vec<u64> = backends.iter().map(EchoBackend::connections).collect();
    Ok(Measured {
        pass: stats.errors == 0
            && stats.requests > 0
            && distribution.iter().all(|count| *count > 0),
        reason: format!(
            "{} echo errors, connections per backend {distribution:?}",
            stats.errors
        ),
        stats,
        distribution,
    })
}

/// A self-signed certificate for [`load::TLS_NAME`] and a client config
/// trusting it that offers `alpn`.
fn certificate(alpn: &[u8]) -> Result<(TlsFiles, Arc<ClientConfig>), Problem> {
    let failed = |err: &dyn fmt::Display| Problem::Failed(format!("certificate: {err}"));
    let certified = rcgen::generate_simple_self_signed(vec![
        load::TLS_NAME.to_owned(),
        String::from("127.0.0.1"),
    ])
    .map_err(|err| failed(&err))?;
    let mut roots = RootCertStore::empty();
    roots
        .add(certified.cert.der().clone())
        .map_err(|err| failed(&err))?;
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![alpn.to_vec()];
    let files = TlsFiles {
        cert_pem: certified.cert.pem(),
        key_pem: certified.signing_key.serialize_pem(),
    };
    Ok((files, Arc::new(config)))
}

/// Requests over TLS, as HTTP/1.1 or HTTP/2 depending on `http2`.
async fn tls(target: &Target, options: Options, http2: bool) -> Result<Measured, Problem> {
    let alpn: &[u8] = if http2 { b"h2" } else { b"http/1.1" };
    let (files, config) = certificate(alpn)?;
    let backends = spawn_backends(&[Duration::ZERO; 2]).await?;
    let mut spec = ProxySpec::http("round_robin", addresses(&backends));
    spec.tls = Some(files);
    let proxy = target.start(&spec).await?;
    let client = Client {
        tls: Some(config),
        ..Client::plain(KeyMode::None)
    };
    let outcome = load::http(load(proxy.address(), options), &client).await;

    let stats = outcome.stats;
    let distribution = hits(&backends);
    let spread =
        stats.errors == 0 && stats.requests > 0 && distribution.iter().all(|count| *count > 0);
    let (pass, reason) = if http2 {
        (
            spread && outcome.http2 == stats.requests,
            format!(
                "{} of {} responses over HTTP/2, {} errors",
                outcome.http2, stats.requests, stats.errors
            ),
        )
    } else {
        (
            spread,
            format!("HTTP/1.1 over TLS, {} errors", stats.errors),
        )
    };
    Ok(Measured {
        pass,
        reason,
        stats,
        distribution,
    })
}

async fn large_body(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 2]).await?;
    let proxy = target
        .start(&ProxySpec::http("round_robin", addresses(&backends)))
        .await?;
    let body: Vec<u8> = (0..LARGE_BODY).map(|index| (index % 251) as u8).collect();
    let client = Client {
        body: Bytes::from(body),
        ..Client::plain(KeyMode::None)
    };
    let outcome = load::http(load(proxy.address(), options), &client).await;

    let stats = outcome.stats;
    let mebibytes_per_second = stats.rps * LARGE_BODY as f64 / (1024.0 * 1024.0);
    Ok(Measured {
        pass: stats.errors == 0 && stats.requests > 0,
        reason: format!(
            "1 MiB up and back per request, {mebibytes_per_second:.0} MiB/s each way, \
             {} errors (wrong echoes included)",
            stats.errors
        ),
        stats,
        distribution: hits(&backends),
    })
}

async fn websocket(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 2]).await?;
    let proxy = target
        .start(&ProxySpec::http("round_robin", addresses(&backends)))
        .await?;
    let stats = load::websocket(load(proxy.address(), options)).await;

    // Backends count each upgrade request as a hit.
    let distribution = hits(&backends);
    Ok(Measured {
        pass: stats.errors == 0
            && stats.requests > 0
            && distribution.iter().all(|count| *count > 0),
        reason: format!(
            "{} frame echo errors, upgrades per backend {distribution:?}",
            stats.errors
        ),
        stats,
        distribution,
    })
}

async fn udp(target: &Target, options: Options) -> Result<Measured, Problem> {
    let mut backends = Vec::new();
    for _ in 0..2 {
        backends.push(
            UdpEchoBackend::spawn()
                .await
                .map_err(|err| Problem::Failed(format!("udp echo: {err}")))?,
        );
    }
    let mut spec = ProxySpec::http("round_robin", backends.iter().map(UdpEchoBackend::address));
    spec.protocol = Protocol::Udp;
    let proxy = target.start(&spec).await?;
    // One session per worker: at least one per backend.
    let options = Options {
        concurrency: options.concurrency.max(2),
        ..options
    };
    let stats = load::udp(load(proxy.address(), options)).await;

    let distribution: Vec<u64> = backends.iter().map(UdpEchoBackend::sessions).collect();
    let loss = error_rate(&stats);
    Ok(Measured {
        // Loopback UDP should not drop, but a datagram may: allow 1%.
        pass: stats.requests > 0 && loss <= 1.0 && distribution.iter().all(|count| *count > 0),
        reason: format!("{loss:.2}% datagrams lost, sessions per backend {distribution:?}"),
        stats,
        distribution,
    })
}

/// Spec for the scenarios that hold `idle` connections next to the load.
fn idle_spec(backends: &[HttpBackend], options: Options) -> ProxySpec {
    let mut spec = ProxySpec::http("round_robin", addresses(backends));
    spec.max_connections =
        Some(options.idle_connections + MEMORY_WARMUP + options.concurrency + 64);
    spec
}

async fn idle(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 2]).await?;
    let proxy = target.start(&idle_spec(&backends, options)).await?;
    let address = proxy.address();

    let idle = load::open_idle(address, options.idle_connections).await;
    let opened = idle.len();
    let outcome = load::http(load(address, options), &Client::plain(KeyMode::None)).await;
    let dropped = idle.probe(address).await;

    let stats = outcome.stats;
    Ok(Measured {
        pass: opened == options.idle_connections
            && dropped == 0
            && stats.errors == 0
            && stats.requests > 0,
        reason: format!(
            "{opened} of {} idle connections opened, {dropped} of them failed after the load, \
             {} load errors",
            options.idle_connections, stats.errors
        ),
        stats,
        distribution: hits(&backends),
    })
}

async fn memory(target: &Target, options: Options) -> Result<Measured, Problem> {
    let backends = spawn_backends(&[Duration::ZERO; 2]).await?;
    let proxy = target.start(&idle_spec(&backends, options)).await?;
    let address = proxy.address();

    let warmup = load::open_idle(address, MEMORY_WARMUP).await;
    let settle = Duration::from_millis(500);
    tokio::time::sleep(settle).await;
    let baseline = proxy.rss().await?;
    let idle = load::open_idle(address, options.idle_connections).await;
    tokio::time::sleep(settle).await;
    let loaded = proxy.rss().await?;
    let opened = idle.len();
    drop(warmup);

    let mebibyte = 1024.0 * 1024.0;
    let per_connection = loaded.saturating_sub(baseline) as f64 / opened.max(1) as f64;
    Ok(Measured {
        pass: opened == options.idle_connections,
        reason: format!(
            "{:.1} KiB per idle connection: RSS {:.1} MiB with {MEMORY_WARMUP}, \
             {:.1} MiB with {} more ({opened} opened)",
            per_connection / 1024.0,
            baseline as f64 / mebibyte,
            loaded as f64 / mebibyte,
            options.idle_connections
        ),
        stats: idle.stats,
        distribution: hits(&backends),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deviation() {
        assert!((max_deviation(&[25, 25, 50], &[0.25, 0.25, 0.5])).abs() < 1e-9);
        assert!((max_deviation(&[30, 70], &[0.5, 0.5]) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn names_are_kebab_case() {
        assert_eq!(Scenario::ActiveHealth.name(), "active-health");
        assert_eq!(Scenario::IpHash.name(), "ip-hash");
        assert_eq!(Scenario::LargeBody.name(), "large-body");
    }

    #[test]
    fn certificate_is_trusted_for_tls_name() {
        let (files, config) = certificate(b"h2").unwrap();
        assert!(files.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(files.key_pem.contains("PRIVATE KEY"));
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec()]);
    }
}
