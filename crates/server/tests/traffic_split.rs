//! Weighted traffic splits between pools (canary releases), end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::time::Duration;

use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn http_requests_follow_pool_weights() {
    let backends = util::counting_backends(3).await;
    let [
        (stable, stable_counters),
        (canary, canary_counters),
        (off, off_counters),
    ] = <[_; 3]>::try_from(backends).ok().unwrap();
    let proxy = util::spawn(&format!(
        r#"
        [upstream.stable]
        backends = ["{stable}"]
        [upstream.canary]
        backends = ["{canary}"]
        [upstream.off]
        backends = ["{off}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = [
            {{ pool = "stable", weight = 90 }},
            {{ pool = "canary", weight = 10 }},
            {{ pool = "off", weight = 0 }},
        ]
        "#
    ))[0];

    for _ in 0..100 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(stable_counters.requests(), 90);
    assert_eq!(canary_counters.requests(), 10);
    assert_eq!(off_counters.requests(), 0);
}

#[tokio::test]
async fn pool_without_available_members_gives_its_share_away() {
    let (stable, stable_counters) = util::counting_backends(1).await.remove(0);
    let dead = util::dead_address();
    let proxy = util::spawn(&format!(
        r#"
        [upstream.stable]
        backends = ["{stable}"]
        [upstream.canary]
        backends = ["{dead}"]
        passive = {{ fail_duration = "1m", max_fails = 1 }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = [{{ pool = "canary" }}, {{ pool = "stable" }}]
        "#
    ))[0];

    // The first turn of the canary fails and marks its only member down.
    assert_eq!(util::get(proxy, "/").await.0, StatusCode::BAD_GATEWAY);
    for _ in 0..10 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(stable_counters.requests(), 10);
}

async fn tcp_round_trip(proxy: SocketAddr) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buffer = vec![0; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("echo answers")
        .unwrap();
    buffer.truncate(read);
    buffer
}

#[tokio::test]
async fn tcp_connections_follow_pool_weights() {
    let blue = util::tcp_echo("blue:").await;
    let green = util::tcp_echo("green:").await;
    let proxy = util::spawn(&format!(
        r#"
        [upstream.blue]
        backends = ["{blue}"]
        [upstream.green]
        backends = ["{green}"]

        [[server]]
        listen = "127.0.0.1:0"
        protocol = "tcp"
        forward = [{{ pool = "blue", weight = 3 }}, {{ pool = "green", weight = 1 }}]
        "#
    ))[0];

    let mut green_replies = 0;
    for _ in 0..8 {
        match tcp_round_trip(proxy).await.as_slice() {
            b"blue:ping" => {}
            b"green:ping" => green_replies += 1,
            other => panic!("unexpected reply {other:?}"),
        }
    }
    assert_eq!(green_replies, 2);
}
