//! `retry_budget` and `hedge_delay`, end to end.

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::{Request, Response, StatusCode};

/// What the slow backend waits before answering `slow`.
const SLOW: Duration = Duration::from_millis(600);

/// Backend answering `slow` after [`SLOW`], and its request count.
async fn slow_backend() -> (SocketAddr, Arc<AtomicUsize>) {
    let requests: Arc<AtomicUsize> = Arc::default();
    let counter = Arc::clone(&requests);
    let address = util::backend_with(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        async {
            tokio::time::sleep(SLOW).await;
            Response::new(Full::from("slow"))
        }
    })
    .await;
    (address, requests)
}

/// Backend that reads the request, waits `delay` and closes the connection.
async fn dropping_backend(delay: Duration) -> SocketAddr {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::task::spawn(async move {
                let mut buffer = [0; 1024];
                let _ = stream.read(&mut buffer).await;
                tokio::time::sleep(delay).await;
            });
        }
    });
    address
}

/// Round robin proxy over `backends` in order, with extra pool `keys`.
fn proxy(backends: &[SocketAddr], keys: &str) -> SocketAddr {
    let list: Vec<String> = backends.iter().map(|b| format!("\"{b}\"")).collect();
    util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        try_interval = "1ms"
        backends = [{}]
        {keys}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#,
        list.join(", ")
    ))[0]
}

#[tokio::test]
async fn slow_request_is_hedged_on_another_backend() {
    let (slow, slow_requests) = slow_backend().await;
    let (fast, fast_counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;

    let started = Instant::now();
    let (status, _, body) = util::get(proxy(&[slow, fast], "hedge_delay = \"50ms\""), "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, fast.to_string(), "the hedge answers first");
    assert!(
        started.elapsed() < SLOW,
        "the slow backend was waited for: {:?}",
        started.elapsed()
    );
    assert_eq!(
        (
            slow_requests.load(Ordering::SeqCst),
            fast_counters.requests()
        ),
        (1, 1)
    );

    // Without the option the same request waits for the slow backend.
    let (_, _, body) = util::get(proxy(&[slow, fast], ""), "/").await;
    assert_eq!(body, "slow");
}

#[tokio::test]
async fn fast_request_is_not_hedged() {
    let (first, first_counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let (second, second_counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;

    let (_, _, body) = util::get(proxy(&[first, second], "hedge_delay = \"300ms\""), "/").await;
    assert_eq!(body, first.to_string());
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        (first_counters.requests(), second_counters.requests()),
        (1, 0)
    );
}

#[tokio::test]
async fn only_retryable_requests_are_hedged() {
    let (slow, _) = slow_backend().await;
    let (fast, fast_counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let post = || {
        Request::post("/")
            .body(Empty::<Bytes>::new())
            .expect("valid request")
    };

    let by_default = proxy(&[slow, fast], "hedge_delay = \"50ms\"");
    assert_eq!(util::send(by_default, post()).await.2, "slow");
    assert_eq!(fast_counters.requests(), 0, "a POST is sent once");

    let listed = proxy(
        &[slow, fast],
        "hedge_delay = \"50ms\"\nretry_match = { method = [\"POST\"] }",
    );
    assert_eq!(util::send(listed, post()).await.2, fast.to_string());
}

#[tokio::test]
async fn hedge_needs_another_backend() {
    let (slow, slow_requests) = slow_backend().await;

    let (status, _, body) = util::get(proxy(&[slow], "hedge_delay = \"50ms\""), "/").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"slow"[..]));
    assert_eq!(
        slow_requests.load(Ordering::SeqCst),
        1,
        "not sent twice to the same backend"
    );
}

#[tokio::test]
async fn hedge_answers_when_the_first_attempt_fails() {
    let dropping = dropping_backend(Duration::from_millis(200)).await;
    let (slow, _) = slow_backend().await;

    // The first attempt fails while the hedge is still waiting: no `retries`,
    // so only the hedge can answer.
    let (status, _, body) =
        util::get(proxy(&[dropping, slow], "hedge_delay = \"50ms\""), "/").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"slow"[..]));

    // Both fail: the error of the first attempt is the answer.
    let other = dropping_backend(Duration::from_millis(100)).await;
    let (status, _, _) = util::get(proxy(&[dropping, other], "hedge_delay = \"50ms\""), "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn hedges_count_against_the_retry_budget() {
    let (slow, _) = slow_backend().await;
    let (fast, fast_counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;

    let spent = proxy(
        &[slow, fast],
        "hedge_delay = \"50ms\"\nretry_budget = { percent = 0, min_retries = 0 }",
    );
    assert_eq!(util::get(spent, "/").await.2, "slow");
    assert_eq!(fast_counters.requests(), 0);

    let within = proxy(
        &[slow, fast],
        "hedge_delay = \"50ms\"\nretry_budget = { percent = 0, min_retries = 1 }",
    );
    assert_eq!(util::get(within, "/").await.2, fast.to_string());
}

#[tokio::test]
async fn retry_budget_bounds_status_and_connect_retries() {
    let failing = util::status_backend(503).await;
    let dead = util::dead_address();
    let (healthy, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let retrying = |backend: SocketAddr, budget: &str| {
        proxy(
            &[backend, healthy],
            &format!("retries = 1\nretry_match = {{ status = [\"503\"] }}\n{budget}"),
        )
    };

    let spent = "retry_budget = { percent = 0, min_retries = 0 }";
    assert_eq!(
        util::get(retrying(failing, spent), "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "the 503 is not retried"
    );
    assert_eq!(
        util::get(retrying(dead, spent), "/").await.0,
        StatusCode::BAD_GATEWAY,
        "the refused connection is not retried"
    );
    assert_eq!(counters.requests(), 0);

    // One retry at a time is allowed, and given back when its request ends:
    // every request in turn starts on the bad backend and is retried.
    let within = "retry_budget = { percent = 0, min_retries = 1 }";
    for backend in [failing, dead] {
        let proxy = retrying(backend, within);
        for _ in 0..3 {
            let (status, _, body) = util::get(proxy, "/").await;
            assert_eq!(
                (status, &body[..]),
                (StatusCode::OK, healthy.to_string().as_bytes())
            );
        }
    }
}

#[tokio::test]
async fn retry_budget_is_shared_by_concurrent_requests() {
    let dropping = dropping_backend(Duration::from_millis(100)).await;
    let also_dropping = dropping_backend(Duration::from_millis(100)).await;
    let (slow, _) = slow_backend().await;
    let (also_slow, _) = slow_backend().await;
    // Round robin gives each request a dropping backend first. One retry is
    // allowed, and it is still waiting for a slow backend when the other
    // request wants to retry.
    let proxy = proxy(
        &[dropping, also_dropping, slow, also_slow],
        "retries = 1\nretry_budget = { percent = 0, min_retries = 1 }",
    );

    let first = tokio::task::spawn(async move { util::get(proxy, "/").await.0 });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let second = util::get(proxy, "/").await.0;
    let mut statuses = [first.await.unwrap(), second];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::BAD_GATEWAY]);
}
