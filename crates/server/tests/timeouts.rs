//! Upstream and client timeouts.

mod util;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Backend that accepts connections and never answers.
async fn silent_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    address
}

/// Reads until the peer closes the connection, failing after 3s.
async fn closed_within_3s(stream: &mut TcpStream) -> Duration {
    let started = Instant::now();
    let mut buffer = [0; 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer))
            .await
            .expect("connection should have been closed");
        if !matches!(read, Ok(read) if read > 0) {
            return started.elapsed();
        }
    }
}

#[tokio::test]
async fn slow_backend_gets_a_504() {
    let backend = silent_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"], response_header_timeout = \"200ms\" }}"
    ))[0];

    let started = Instant::now();
    let (status, _, _) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn upstream_read_timeout_fails_the_request() {
    let backend = silent_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"], read_timeout = \"200ms\" }}"
    ))[0];

    let started = Instant::now();
    let (status, _, _) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn idle_tcp_connections_are_closed() {
    let echo = util::tcp_echo("").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nidle_timeout = \"400ms\"\nforward = \"{echo}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    // Traffic keeps the connection open past the idle time.
    for _ in 0..5 {
        stream.write_all(b"x").await.expect("still open");
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).await.expect("echo");
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    closed_within_3s(&mut stream).await;
}

#[tokio::test]
async fn tcp_stream_timeout_caps_the_lifetime() {
    let echo = util::tcp_echo("").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nstream_timeout = \"300ms\"\nforward = \"{echo}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    let started = Instant::now();
    loop {
        if stream.write_all(b"x").await.is_err() {
            break;
        }
        let mut byte = [0; 1];
        if !matches!(stream.read(&mut byte).await, Ok(1)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(started.elapsed() < Duration::from_secs(3), "never closed");
    }
    assert!(started.elapsed() >= Duration::from_millis(250));
}

#[tokio::test]
async fn upgraded_tunnels_end_after_stream_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let backend = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let mut head = [0; 4096];
        let _ = stream.read(&mut head).await.expect("request");
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
            )
            .await
            .expect("101");
        // Hold the tunnel open.
        let _ = stream.read(&mut head).await;
    });

    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nstream_timeout = \"300ms\"\nforward = \"{backend}\""
    ))[0];
    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream
        .write_all(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
        )
        .await
        .expect("request");
    let mut head = [0; 4096];
    let read = stream.read(&mut head).await.expect("101");
    assert!(String::from_utf8_lossy(&head[..read]).starts_with("HTTP/1.1 101"));

    let elapsed = closed_within_3s(&mut stream).await;
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
}

#[tokio::test]
async fn partial_request_heads_are_cut_off() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nread_header_timeout = \"200ms\"\nforward = \"{backend}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.write_all(b"GET / HT").await.expect("partial line");
    closed_within_3s(&mut stream).await;

    // Complete requests are unaffected.
    let (status, _, body) = util::get(proxy, "/").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"ok"[..]));
}

#[tokio::test]
async fn idle_http_connections_are_closed_but_busy_ones_are_not() {
    let backend = silent_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nidle_timeout = \"200ms\"\nforward = {{ backends = [\"{backend}\"], response_header_timeout = \"700ms\" }}"
    ))[0];

    // A request waiting for a slow backend outlives the idle time.
    let (status, _, _) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);

    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    let elapsed = closed_within_3s(&mut stream).await;
    assert!(elapsed >= Duration::from_millis(150), "{elapsed:?}");
}
