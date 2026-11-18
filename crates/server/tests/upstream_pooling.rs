//! Upstream connection handling of `http/transport.rs`: the shared h2c
//! connection, idle pool limits and expiry.

mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// HTTP/2 cleartext backend answering with the request version and URI.
/// `close` drops every open connection. Returns the address and the number
/// of accepted connections.
async fn h2c_backend(close: Arc<Notify>) -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            let close = Arc::clone(&close);
            tokio::task::spawn(async move {
                let service = service_fn(|request: Request<Incoming>| async move {
                    let body = format!("{:?} {}", request.version(), request.uri());
                    Ok::<_, Infallible>(Response::new(Full::<Bytes>::from(body)))
                });
                let connection = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service);
                tokio::select! {
                    _ = connection => {}
                    () = close.notified() => {}
                }
            });
        }
    });
    (address, connections)
}

fn proxy(forward_options: &str, backend: SocketAddr) -> SocketAddr {
    util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"]{forward_options} }}"
    ))[0]
}

async fn body_of(proxy: SocketAddr, uri: &str) -> String {
    let (status, _, body) = util::get(proxy, uri).await;
    assert_eq!(status, StatusCode::OK);
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn h2c_shares_one_connection_and_redials_once_it_closes() {
    let close = Arc::new(Notify::new());
    let (backend, connections) = h2c_backend(Arc::clone(&close)).await;
    let proxy = proxy(", h2c = true", backend);

    for _ in 0..5 {
        let body = body_of(proxy, "/path?q=1").await;
        // HTTP/2 to the backend, with an absolute URI.
        assert!(body.starts_with("HTTP/2.0 http://"), "{body}");
        assert!(body.ends_with("/path?q=1"), "{body}");
    }
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "one shared connection"
    );

    // Once the backend drops it, the next request opens a new one.
    close.notify_waiters();
    util::eventually(async || {
        let (status, ..) = util::get(proxy, "/again").await;
        status == StatusCode::OK && connections.load(Ordering::SeqCst) == 2
    })
    .await;
    body_of(proxy, "/again").await;
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn h2c_backend_that_is_down_is_a_bad_gateway() {
    let proxy = proxy(", h2c = true", util::dead_address());
    let (status, ..) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_pooling_opens_one_connection_per_request() {
    for options in [
        ", keepalive = \"0s\"",
        ", keepalive_idle_conns_per_host = 0",
    ] {
        let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
        let proxy = proxy(options, backend);
        for _ in 0..4 {
            assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
        }
        assert_eq!(counters.requests(), 4, "{options}");
        assert_eq!(counters.connections(), 4, "{options}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_connections_expire_after_keepalive() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = proxy(", keepalive = \"100ms\"", backend);

    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(counters.connections(), 1, "reused within keepalive");

    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(counters.connections(), 2, "expired connection not reused");
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_connections_closed_by_read_timeout_are_replaced() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = proxy(", read_timeout = \"100ms\"", backend);

    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    // The pooled connection times out while idle; the next request must not
    // fail on it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(counters.requests(), 2);
    assert_eq!(counters.connections(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_pool_keeps_at_most_the_configured_connections() {
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let backend = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::task::spawn(async move {
                // Slow enough that a burst needs one connection per request.
                let service = service_fn(|_| async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok::<_, Infallible>(Response::new(Full::<Bytes>::from("slow")))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    let proxy = proxy(", keepalive_idle_conns_per_host = 1", backend);

    let burst = async || {
        let requests: Vec<_> = (0..3)
            .map(|_| tokio::task::spawn(util::get(proxy, "/")))
            .collect();
        for request in requests {
            assert_eq!(request.await.expect("join").0, StatusCode::OK);
        }
    };
    burst().await;
    assert_eq!(connections.load(Ordering::SeqCst), 3);
    // Let the finished connections return to the pool.
    tokio::time::sleep(Duration::from_millis(100)).await;
    burst().await;
    assert_eq!(
        connections.load(Ordering::SeqCst),
        5,
        "only one idle connection was kept for the second burst"
    );
}
