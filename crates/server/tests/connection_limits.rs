//! `max_connections` per backend and the queue of a pool, end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Backend taking `delay` per request, and the most requests it had at once.
async fn slow_backend(delay: Duration) -> (SocketAddr, Arc<AtomicUsize>) {
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&peak);
    let address = util::backend_with(move |_| {
        let (active, peak) = (Arc::clone(&active), Arc::clone(&seen));
        async move {
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            active.fetch_sub(1, Ordering::SeqCst);
            Response::new(Full::new(Bytes::from_static(b"ok")))
        }
    })
    .await;
    (address, peak)
}

fn proxy(backend: SocketAddr, keys: &str) -> SocketAddr {
    util::spawn(&format!(
        "[upstream.api]\nbackends = [\"{backend}\"]\n{keys}\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@api\"\n"
    ))[0]
}

/// Statuses of `count` requests sent at once.
async fn burst(proxy: SocketAddr, count: usize) -> Vec<StatusCode> {
    let requests: Vec<_> = (0..count)
        .map(|_| tokio::task::spawn(async move { util::get(proxy, "/").await.0 }))
        .collect();
    let mut statuses = Vec::with_capacity(count);
    for request in requests {
        statuses.push(request.await.unwrap());
    }
    statuses
}

#[tokio::test]
async fn queued_requests_are_served_one_at_a_time() {
    let (backend, peak) = slow_backend(Duration::from_millis(100)).await;
    let proxy = proxy(backend, "max_connections = 1\nqueue_size = 8");

    assert_eq!(burst(proxy, 5).await, [StatusCode::OK; 5]);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn without_a_queue_requests_over_the_limit_get_503() {
    let (backend, peak) = slow_backend(Duration::from_millis(200)).await;
    let proxy = proxy(backend, "max_connections = 2");

    let statuses = burst(proxy, 5).await;
    let served = statuses.iter().filter(|status| **status == StatusCode::OK);
    assert_eq!(served.count(), 2, "{statuses:?}");
    assert!(
        statuses
            .iter()
            .all(|status| matches!(*status, StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE)),
        "{statuses:?}"
    );
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    // The slots are free again once the requests are over.
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
}

#[tokio::test]
async fn queue_refuses_when_full_and_times_out() {
    let (backend, peak) = slow_backend(Duration::from_millis(800)).await;
    let proxy = proxy(
        backend,
        "max_connections = 1\nqueue_size = 1\nqueue_timeout = \"150ms\"",
    );

    let first = tokio::task::spawn(async move { util::get(proxy, "/").await.0 });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = Instant::now();
    let queued = tokio::task::spawn(async move { util::get(proxy, "/").await.0 });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The queue holds one request: this one is refused without waiting.
    assert_eq!(
        util::get(proxy, "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(started.elapsed() < Duration::from_millis(140));

    assert_eq!(queued.await.unwrap(), StatusCode::SERVICE_UNAVAILABLE);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(150) && waited < Duration::from_millis(700),
        "queued request answered after {waited:?}"
    );
    assert_eq!(first.await.unwrap(), StatusCode::OK);
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn tcp_connections_queue_for_a_backend() {
    let backend = util::tcp_echo("echo:").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\n\
         forward = {{ backends = [\"{backend}\"], max_connections = 1, queue_size = 1, queue_timeout = \"5s\" }}"
    ))[0];
    let round_trip = async |stream: &mut TcpStream| {
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0; 9];
        stream.read_exact(&mut reply).await.map(|_| reply)
    };

    let mut first = TcpStream::connect(proxy).await.unwrap();
    assert_eq!(&round_trip(&mut first).await.unwrap(), b"echo:ping");

    // The second connection waits for the first to end.
    let mut second = TcpStream::connect(proxy).await.unwrap();
    let waiting = tokio::time::timeout(Duration::from_millis(200), round_trip(&mut second)).await;
    assert!(waiting.is_err(), "served over the limit: {waiting:?}");

    // The queue is full: the third is closed at once.
    let mut third = TcpStream::connect(proxy).await.unwrap();
    assert!(round_trip(&mut third).await.is_err());

    drop(first);
    let mut reply = [0; 9];
    second.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"echo:ping");
}
