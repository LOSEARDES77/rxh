//! Request mirroring (shadow traffic), end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, Version};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// Sends raw HTTP/1.1 bytes on a new connection and returns the response.
async fn raw(proxy: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .expect("the proxy answers")
        .unwrap();
    response
}

fn post(uri: &str, body: &str) -> String {
    format!(
        "POST {uri} HTTP/1.1\r\nHost: app.test\r\nConnection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn requests_are_copied_and_mirror_responses_dropped() {
    let (primary, _) = util::echo_backend().await;
    let (shadow, mut copies) = util::body_interceptor(500).await;
    let proxy = util::spawn(&format!(
        r#"
        [upstream.shadow]
        backends = ["{shadow}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "{primary}"
        request = {{ headers = {{ set = {{ x-env = "prod" }} }} }}
        mirror = {{ pool = "shadow" }}
        "#
    ))[0];

    let response = raw(proxy, &post("/orders?id=7", "hello mirror")).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("hello mirror"), "{response}");

    let (parts, body) = tokio::time::timeout(Duration::from_secs(5), copies.recv())
        .await
        .expect("the mirror gets a copy")
        .unwrap();
    assert_eq!(parts.method, "POST");
    assert_eq!(parts.uri, "/orders?id=7");
    assert_eq!(parts.headers["host"], "app.test");
    assert_eq!(parts.headers["x-env"], "prod");
    assert_eq!(parts.headers["x-forwarded-for"], "127.0.0.1");
    assert_eq!(body, "hello mirror");
}

#[tokio::test]
async fn percent_copies_an_even_share() {
    let (primary, primary_counters) = util::counting_backends(1).await.remove(0);
    let (shadow, shadow_counters) = util::counting_backends(1).await.remove(0);
    let (off, off_counters) = util::counting_backends(1).await.remove(0);
    let proxy = util::spawn(&format!(
        r#"
        [upstream.primary]
        backends = ["{primary}"]
        [upstream.shadow]
        backends = ["{shadow}"]
        [upstream.off]
        backends = ["{off}"]

        [[server]]
        listen = "127.0.0.1:0"
        match = [{{
            forward = "@primary",
            mirror = [{{ pool = "shadow", percent = 10 }}, {{ pool = "off", percent = 0 }}],
        }}]
        "#
    ))[0];

    for _ in 0..50 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }
    assert_eq!(primary_counters.requests(), 50);
    util::eventually(async || shadow_counters.requests() == 5).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(shadow_counters.requests(), 5);
    assert_eq!(off_counters.requests(), 0);
}

#[tokio::test]
async fn slow_or_dead_mirrors_do_not_delay_the_client() {
    let primary = util::backend("primary").await;
    let slow = util::backend_with(|_| async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        Response::new(Full::<Bytes>::from("late"))
    })
    .await;
    let dead = util::dead_address();
    let proxy = util::spawn(&format!(
        r#"
        [upstream.slow]
        backends = ["{slow}"]
        [upstream.dead]
        backends = ["{dead}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "{primary}"
        mirror = [{{ pool = "slow" }}, {{ pool = "dead" }}]
        "#
    ))[0];

    let started = Instant::now();
    for _ in 0..3 {
        let (status, _, body) = util::get(proxy, "/").await;
        assert_eq!(
            (status, body.as_ref()),
            (StatusCode::OK, b"primary".as_ref())
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the client must not wait for mirrors, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn large_and_unknown_length_bodies_are_not_copied() {
    let (primary, _) = util::echo_backend().await;
    let (shadow, mut copies) = util::body_interceptor(200).await;
    let proxy = util::spawn(&format!(
        r#"
        [upstream.shadow]
        backends = ["{shadow}"]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "{primary}"
        mirror = {{ pool = "shadow", max_body_size = 8 }}
        "#
    ))[0];

    // Over the limit: forwarded whole, not copied.
    let response = raw(proxy, &post("/big", "0123456789")).await;
    assert!(response.ends_with("0123456789"), "{response}");
    // Chunked: the length is unknown, so not copied either.
    let chunked = "POST /chunked HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
                   Transfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
    let response = raw(proxy, chunked).await;
    assert!(response.ends_with("abc"), "{response}");
    // Within the limit: copied. It is the first copy the mirror sees.
    raw(proxy, &post("/small", "01234567")).await;

    let (parts, body) = tokio::time::timeout(Duration::from_secs(5), copies.recv())
        .await
        .expect("the mirror gets the small request")
        .unwrap();
    assert_eq!(parts.uri, "/small");
    assert_eq!(body, "01234567");
}

/// HTTP/2 cleartext backend sending the version and URI of each request
/// to the returned channel.
async fn h2c_interceptor() -> (SocketAddr, mpsc::Receiver<(Version, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel(16);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let sender = sender.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let sender = sender.clone();
                async move {
                    let _ = sender
                        .send((request.version(), request.uri().to_string()))
                        .await;
                    Ok::<_, Infallible>(Response::new(Full::<Bytes>::default()))
                }
            });
            tokio::spawn(
                hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service),
            );
        }
    });
    (address, receiver)
}

#[tokio::test]
async fn h2c_mirror_gets_an_http2_copy() {
    let primary = util::backend("primary").await;
    let (shadow, mut copies) = h2c_interceptor().await;
    let proxy = util::spawn(&format!(
        r#"
        [upstream.shadow]
        backends = ["{shadow}"]
        h2c = true

        [[server]]
        listen = "127.0.0.1:0"
        forward = "{primary}"
        mirror = {{ pool = "shadow" }}
        "#
    ))[0];

    let (status, _, body) = util::get(proxy, "/h2?x=1").await;
    assert_eq!(
        (status, body.as_ref()),
        (StatusCode::OK, b"primary".as_ref())
    );
    let (version, uri) = tokio::time::timeout(Duration::from_secs(5), copies.recv())
        .await
        .expect("the h2c mirror gets a copy")
        .unwrap();
    assert_eq!(version, Version::HTTP_2);
    assert!(uri.ends_with("/h2?x=1"), "{uri}");
}
