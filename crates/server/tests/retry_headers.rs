//! `retry_match` and the `add`, `replace` and wildcard `remove` header rules.

mod util;

use std::sync::Arc;
use std::sync::atomic::AtomicU16;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, StatusCode, header};
use rxh_server::config::Config;

fn proxy_with_retry(backends: &[std::net::SocketAddr], retry_match: &str) -> std::net::SocketAddr {
    let list: Vec<String> = backends.iter().map(|b| format!("\"{b}\"")).collect();
    util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        retries = 1
        try_interval = "1ms"
        retry_match = {retry_match}
        backends = [{}]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#,
        list.join(", ")
    ))[0]
}

#[tokio::test]
async fn status_match_retries_another_backend() {
    let failing = util::status_backend(503).await;
    let (healthy, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        retries = 1
        try_interval = "1ms"
        retry_match = {{ status = ["503"] }}
        backends = ["{failing}", "{healthy}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ));

    let (status, _, body) = util::get(proxies[0], "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, healthy.to_string());
    assert_eq!(counters.requests(), 1);

    util::eventually(async || {
        let pool = registry.get("api").unwrap();
        pool.pool
            .status()
            .upstreams
            .iter()
            .all(|u| u.num_requests == 0)
    })
    .await;
}

/// Backend that waits `delay` before answering 503.
async fn slow_failing_backend(delay: std::time::Duration) -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                let _ = stream
                    .write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    address
}

#[tokio::test]
async fn try_duration_bounds_status_retries() {
    let slow = slow_failing_backend(std::time::Duration::from_millis(200)).await;
    let (healthy, _) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let config = |try_duration: &str| {
        util::spawn(&format!(
            r#"
            [upstream.api]
            policy = "round_robin"
            try_duration = "{try_duration}"
            try_interval = "1ms"
            retry_match = {{ status = ["503"] }}
            backends = ["{slow}", "{healthy}"]

            [[server]]
            listen = "127.0.0.1:0"
            forward = "@api"
            "#
        ))[0]
    };

    // The first attempt outlasts the budget: its 503 is final.
    assert_eq!(
        util::get(config("100ms"), "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(util::get(config("5s"), "/").await.0, StatusCode::OK);
}

#[tokio::test]
async fn last_attempt_is_returned_when_retries_run_out() {
    let first = util::status_backend(503).await;
    let second = util::status_backend(502).await;
    let proxy = proxy_with_retry(&[first, second], r#"{ status = ["5xx"] }"#);

    let (status, _, _) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "second attempt is final");
}

#[tokio::test]
async fn post_is_not_retried_unless_listed() {
    let failing = util::status_backend(503).await;
    let (healthy, _) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let post = || {
        Request::post("/")
            .body(Empty::<Bytes>::new())
            .expect("valid request")
    };

    let by_default = proxy_with_retry(&[failing, healthy], r#"{ status = ["503"] }"#);
    assert_eq!(
        util::send(by_default, post()).await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );

    let listed = proxy_with_retry(
        &[failing, healthy],
        r#"{ method = ["POST"], status = ["503"] }"#,
    );
    assert_eq!(util::send(listed, post()).await.0, StatusCode::OK);
}

#[tokio::test]
async fn header_rules_add_remove_wildcard_and_replace() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        r#"
        [[server]]
        listen = "127.0.0.1:0"
        forward = "{backend}"
        request.headers.add = {{ "x-multi" = "second" }}
        request.headers.remove = ["x-debug-*"]
        request.headers.replace = {{ "x-path" = {{ search = "^/v1/(.*)$", replace = "/v2/$1" }} }}
        response.headers.add = {{ "x-response-add" = "added" }}
        response.headers.replace = {{ "x-response-remove" = {{ search = "up(stream)", replace = "down-$1" }} }}
        "#
    ))[0];

    let request = Request::get("/")
        .header("x-multi", "first")
        .header("x-debug-a", "1")
        .header("x-debug-b", "1")
        .header("x-keep", "1")
        .header("x-path", "/v1/users")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let (status, headers, _) = util::send(proxy, request).await;
    let upstream = requests.recv().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    let multi: Vec<_> = upstream.headers.get_all("x-multi").iter().collect();
    assert_eq!(multi, ["first", "second"]);
    assert!(!upstream.headers.contains_key("x-debug-a"));
    assert!(!upstream.headers.contains_key("x-debug-b"));
    assert_eq!(upstream.headers["x-keep"], "1");
    assert_eq!(upstream.headers["x-path"], "/v2/users");
    assert_eq!(headers["x-response-add"], "added");
    assert_eq!(headers["x-response-remove"], "down-stream");
    assert!(headers.contains_key(header::SERVER));
}

#[test]
fn invalid_replace_regex_is_a_configuration_error() {
    let text = r#"
        [[server]]
        listen = "127.0.0.1:0"
        forward = "127.0.0.1:1"
        request.headers.replace = { "x" = { search = "(", replace = "" } }
        "#;
    assert!(Config::from_toml(text).is_err());
}

/// Backend that accepts, waits `delay` and drops the connection.
async fn dropping_backend(delay: std::time::Duration) -> std::net::SocketAddr {
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

#[tokio::test]
async fn try_duration_bounds_round_trip_retries() {
    let dropping = dropping_backend(std::time::Duration::from_millis(200)).await;
    let (healthy, _) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let config = |try_duration: &str| {
        util::spawn(&format!(
            r#"
            [upstream.api]
            policy = "round_robin"
            try_duration = "{try_duration}"
            try_interval = "1ms"
            backends = ["{dropping}", "{healthy}"]

            [[server]]
            listen = "127.0.0.1:0"
            forward = "@api"
            "#
        ))[0]
    };

    assert_eq!(
        util::get(config("100ms"), "/").await.0,
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(util::get(config("5s"), "/").await.0, StatusCode::OK);
}
