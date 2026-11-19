//! Slow start of backends added at runtime, end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::time::Duration;

use hyper::StatusCode;
use rxh_core::config::Backend;

/// Answers of `count` requests that came from the backend answering `body`.
async fn answers_from(proxy: SocketAddr, body: &[u8], count: usize) -> usize {
    let mut from = 0;
    for _ in 0..count {
        let (code, _, answer) = util::get(proxy, "/").await;
        assert_eq!(code, StatusCode::OK);
        from += usize::from(answer[..] == *body);
    }
    from
}

#[tokio::test]
async fn added_backend_ramps_up_to_its_full_share() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.api]
        policy = "round_robin"
        backends = ["{old}"]
        slow_start = "1s"

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ));
    let proxy = proxies[0];

    registry
        .get("api")
        .unwrap()
        .pool
        .add(Backend::new(new.to_string()))
        .unwrap();

    // Right after it joined, the new backend gets almost nothing instead of
    // every second request.
    let early = answers_from(proxy, b"new", 20).await;
    assert!(early <= 3, "{early} of 20 requests went to the new backend");

    // Once the ramp is over, round robin alternates again.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(answers_from(proxy, b"new", 20).await, 10);
}
