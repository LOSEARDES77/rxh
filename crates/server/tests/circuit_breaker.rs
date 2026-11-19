//! Circuit breaker on the error ratio or the latency of a backend, end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Response, StatusCode};
use rxh_core::pool::CircuitState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Backend answering `flaky` with the status in `status`, after `delay_ms`.
async fn flaky_backend(status: Arc<AtomicU16>, delay_ms: Arc<AtomicU64>) -> SocketAddr {
    util::backend_with(move |_| {
        let status = status.load(Ordering::SeqCst);
        let delay = Duration::from_millis(delay_ms.load(Ordering::SeqCst));
        async move {
            tokio::time::sleep(delay).await;
            let mut response = Response::new(Full::from(Bytes::from_static(b"flaky")));
            *response.status_mut() = StatusCode::from_u16(status).expect("valid status");
            response
        }
    })
    .await
}

#[tokio::test]
async fn error_ratio_opens_the_breaker_and_a_trial_request_closes_it() {
    let status = Arc::new(AtomicU16::new(500));
    let flaky = flaky_backend(Arc::clone(&status), Arc::default()).await;
    let stable = util::backend("stable").await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.api]
        policy = "first"
        backends = ["{flaky}", "{stable}"]
        circuit_breaker = {{ error_ratio = 0.5, min_requests = 4, open_duration = "300ms" }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ));
    let proxy = proxies[0];
    let circuit = || registry.get("api").unwrap().pool.upstreams()[0].circuit();

    // Below `min_requests` the errors reach the client.
    for _ in 0..4 {
        assert_eq!(circuit(), Some(CircuitState::Closed));
        let (code, _, body) = util::get(proxy, "/").await;
        assert_eq!(
            (code, &body[..]),
            (StatusCode::INTERNAL_SERVER_ERROR, &b"flaky"[..])
        );
    }

    // Open: the failing backend is skipped, even after it recovers.
    assert_eq!(circuit(), Some(CircuitState::Open));
    let pool = serde_json::to_value(registry.get("api").unwrap().pool.status()).unwrap();
    assert_eq!(pool["upstreams"][0]["circuit"], "open");
    assert_eq!(pool["upstreams"][0]["healthy"], false);
    assert_eq!(pool["upstreams"][1]["circuit"], "closed");
    status.store(200, Ordering::SeqCst);
    for _ in 0..3 {
        let (code, _, body) = util::get(proxy, "/").await;
        assert_eq!((code, &body[..]), (StatusCode::OK, &b"stable"[..]));
    }

    // Half open after `open_duration`: one good answer closes the breaker.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(circuit(), Some(CircuitState::HalfOpen));
    for _ in 0..3 {
        let (code, _, body) = util::get(proxy, "/").await;
        assert_eq!((code, &body[..]), (StatusCode::OK, &b"flaky"[..]));
        assert_eq!(circuit(), Some(CircuitState::Closed));
    }
}

#[tokio::test]
async fn latency_opens_the_breaker_and_a_slow_trial_reopens_it() {
    let delay = Arc::new(AtomicU64::new(150));
    let slow = flaky_backend(Arc::new(AtomicU16::new(200)), Arc::clone(&delay)).await;
    let proxy = util::spawn(&format!(
        r#"
        [[server]]
        listen = "127.0.0.1:0"

        [server.forward]
        backends = ["{slow}"]
        circuit_breaker = {{ latency = "50ms", min_requests = 2, open_duration = "300ms" }}
        "#
    ))[0];

    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    // The only backend is out: rxh answers itself.
    assert_eq!(
        util::get(proxy, "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // The trial request is served, but being slow it reopens the breaker.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    assert_eq!(
        util::get(proxy, "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    // A fast trial closes it for good.
    delay.store(0, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(350)).await;
    for _ in 0..3 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
}

async fn tcp_reply(proxy: SocketAddr) -> String {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut reply = [0; 7];
    stream.read_exact(&mut reply).await.unwrap();
    String::from_utf8_lossy(&reply).into_owned()
}

#[tokio::test]
async fn tcp_connections_count_as_successes_and_refused_ones_open_the_breaker() {
    // Serves three connections, then stops listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dying = listener.local_addr().unwrap();
    let served = tokio::task::spawn(async move {
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"dying:x").await.unwrap();
        }
    });
    let alive = util::tcp_echo("alive:").await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.db]
        policy = "first"
        retries = 5
        try_interval = "1ms"
        backends = ["{dying}", "{alive}"]
        circuit_breaker = {{ error_ratio = 0.5, min_requests = 4, open_duration = "1m" }}

        [[server]]
        listen = "127.0.0.1:0"
        protocol = "tcp"
        forward = "@db"
        "#
    ));
    let upstreams = registry.get("db").unwrap().pool.upstreams();

    for _ in 0..3 {
        assert_eq!(tcp_reply(proxies[0]).await, "dying:x");
    }
    served.await.unwrap();
    assert_eq!(upstreams[0].circuit(), Some(CircuitState::Closed));

    // The next connection is retried on the dead backend until half of its
    // six connections failed; from then on it is left alone.
    for _ in 0..4 {
        assert_eq!(tcp_reply(proxies[0]).await, "alive:x");
        assert_eq!(upstreams[0].circuit(), Some(CircuitState::Open));
    }
    assert_eq!(upstreams[0].total_failures(), 3);
    assert_eq!(upstreams[1].circuit(), Some(CircuitState::Closed));
}
