//! Load balancing policies, retries and health checks, end to end.

mod util;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::header;
use hyper::{Request, StatusCode};

#[tokio::test]
async fn weighted_round_robin_matches_weights() {
    let backends = util::counting_backends(3).await;
    let weights = [1, 3, 2];
    let list: Vec<String> = backends
        .iter()
        .zip(weights)
        .map(|((address, _), weight)| format!("{{ address = \"{address}\", weight = {weight} }}"))
        .collect();
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = [{}]",
        list.join(", ")
    ))[0];

    for cycle in 1..=5 {
        for _ in 0..6 {
            util::get(proxy, "/").await;
        }
        for ((_, counters), weight) in backends.iter().zip(weights) {
            assert_eq!(counters.requests(), weight * cycle);
        }
    }
}

#[tokio::test]
async fn retries_skip_dead_backends() {
    let (alive, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let dead = util::dead_address();
    let proxy = util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        retries = 2
        try_interval = "1ms"
        backends = ["{dead}", "{alive}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ))[0];

    for _ in 0..10 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(counters.requests(), 10);
}

#[tokio::test]
async fn passive_health_takes_failing_backend_out() {
    let failing = util::status_backend(503).await;
    let (healthy, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "first"
        backends = ["{failing}", "{healthy}"]
        passive = {{ fail_duration = "1m", max_fails = 2, unhealthy_status = ["5xx"] }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ))[0];

    assert_eq!(
        util::get(proxy, "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        util::get(proxy, "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    for _ in 0..5 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(counters.requests(), 5);
}

#[tokio::test]
async fn active_health_checks_remove_and_restore_backends() {
    let health = Arc::new(AtomicU16::new(200));
    let (flaky, flaky_counters) = util::counting_backend(Arc::clone(&health)).await;
    let (stable, _) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        backends = ["{flaky}", "{stable}"]
        health = {{ uri = "/health", interval = "50ms", timeout = "1s", body = "^health$" }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ));
    let proxy = proxies[0];
    let _checks = registry.start_health_checks();

    health.store(503, Ordering::SeqCst);
    let pool = registry.get("api").unwrap();
    util::eventually(async || !pool.pool.upstreams()[0].is_healthy()).await;

    let before = flaky_counters.requests();
    for _ in 0..10 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(
        flaky_counters.requests(),
        before,
        "unhealthy backend must get no traffic"
    );

    health.store(200, Ordering::SeqCst);
    util::eventually(async || pool.pool.upstreams()[0].is_healthy()).await;
    for _ in 0..4 {
        util::get(proxy, "/").await;
    }
    assert!(flaky_counters.requests() > before);
}

#[tokio::test]
async fn hash_policies_are_sticky() {
    let backends = util::counting_backends(4).await;
    let list: Vec<String> = backends
        .iter()
        .map(|(address, _)| format!("\"{address}\""))
        .collect();
    let list = list.join(", ");
    let proxies = util::spawn(&format!(
        r#"
        [[server]]
        listen = "127.0.0.1:0"
        forward = {{ policy = "uri_hash", backends = [{list}] }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = {{ policy = "header", key = "x-user", backends = [{list}] }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = {{ policy = "query", key = "user", backends = [{list}] }}
        "#
    ));

    let mut owners = HashSet::new();
    for path in ["/a", "/b", "/c", "/d", "/e", "/f", "/g", "/h"] {
        let (_, _, first) = util::get(proxies[0], path).await;
        let (_, _, second) = util::get(proxies[0], path).await;
        assert_eq!(first, second, "uri_hash must be sticky for {path}");
        owners.insert(first);
    }
    assert!(owners.len() > 1, "uri_hash should spread keys");

    for user in ["alice", "bob", "carol"] {
        let request = || {
            Request::get("/")
                .header("x-user", user)
                .body(Empty::<Bytes>::new())
                .unwrap()
        };
        let (_, _, first) = util::send(proxies[1], request()).await;
        let (_, _, second) = util::send(proxies[1], request()).await;
        assert_eq!(first, second, "header policy must be sticky for {user}");

        let uri = format!("/?user={user}");
        assert_eq!(
            util::get(proxies[2], &uri).await.2,
            util::get(proxies[2], &uri).await.2
        );
    }
}

#[tokio::test]
async fn cookie_policy_pins_clients() {
    let backends = util::counting_backends(3).await;
    let list: Vec<String> = backends
        .iter()
        .map(|(address, _)| format!("\"{address}\""))
        .collect();
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ policy = \"cookie\", secret = \"s\", backends = [{}] }}",
        list.join(", ")
    ))[0];

    let (_, headers, owner) = util::get(proxy, "/").await;
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    let pair = cookie.split(';').next().unwrap().to_owned();
    assert!(pair.starts_with("lb="));

    for _ in 0..10 {
        let request = Request::get("/")
            .header(header::COOKIE, &pair)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let (_, headers, body) = util::send(proxy, request).await;
        assert_eq!(body, owner);
        assert!(!headers.contains_key(header::SET_COOKIE));
    }
}

#[tokio::test]
async fn least_conn_avoids_busy_backend() {
    use tokio::io::AsyncWriteExt;

    let backends = util::counting_backends(2).await;
    let (busy, idle) = (backends[0].0, backends[1].0);
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = {{ policy = \"first\", backends = [\"{busy}\"] }}\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = {{ policy = \"least_conn\", backends = [\"{busy}\", \"{idle}\"] }}"
    ));

    // Hold a TCP connection through the least_conn pool, then check that new
    // connections go to the other backend.
    let mut held = tokio::net::TcpStream::connect(proxy[1]).await.unwrap();
    held.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let held_backend = if backends[0].1.connections() == 1 {
        0
    } else {
        1
    };

    for _ in 0..5 {
        let mut stream = tokio::net::TcpStream::connect(proxy[1]).await.unwrap();
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
    }

    assert_eq!(
        backends[held_backend].1.connections(),
        1,
        "busy backend got new connections"
    );
    drop(held);
}

#[tokio::test]
async fn upstreams_can_be_added_and_removed_live() {
    let backends = util::counting_backends(2).await;
    let (first, second) = (backends[0].0, backends[1].0);
    let (proxies, registry) = util::spawn_with_registry(&format!(
        "[upstream.api]\npolicy = \"round_robin\"\nbackends = [\"{first}\"]\n[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@api\""
    ));

    let pool = registry.get("api").unwrap();
    pool.pool
        .add(rxh_server::config::Backend::new(second.to_string()))
        .unwrap();
    for _ in 0..4 {
        util::get(proxies[0], "/").await;
    }
    assert_eq!(backends[1].1.requests(), 2);

    assert!(pool.pool.remove(&first.to_string()));
    for _ in 0..4 {
        util::get(proxies[0], "/").await;
    }
    assert_eq!(backends[0].1.requests(), 2);
    assert_eq!(backends[1].1.requests(), 6);
}
