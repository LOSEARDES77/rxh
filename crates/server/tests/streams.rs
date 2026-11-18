//! `flush_interval` on pools and `stream_close_delay` on servers.

mod util;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Backend that answers every request with `response` and then keeps the
/// connection open without sending more.
async fn stalling_backend(response: &'static [u8]) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut head = [0; 4096];
            let _ = stream.read(&mut head).await;
            let _ = stream.write_all(response).await;
            held.push(stream);
        }
    });
    address
}

/// Time from sending a request through `proxy` to the first body byte.
async fn first_body_byte(proxy: SocketAddr) -> Duration {
    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    let started = Instant::now();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("request");
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer))
            .await
            .expect("no body byte within 3s")
            .expect("read");
        assert!(
            read > 0,
            "closed early: {}",
            String::from_utf8_lossy(&received)
        );
        received.extend_from_slice(&buffer[..read]);
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n")
            && received.len() > end + 4
        {
            return started.elapsed();
        }
    }
}

fn proxy_with(backend: SocketAddr, options: &str) -> SocketAddr {
    util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"]{options} }}"
    ))[0]
}

const PARTIAL: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\na";

#[tokio::test]
async fn responses_stream_immediately_by_default() {
    let backend = stalling_backend(PARTIAL).await;
    let elapsed = first_body_byte(proxy_with(backend, "")).await;
    assert!(elapsed < Duration::from_millis(400), "{elapsed:?}");
}

#[tokio::test]
async fn flush_interval_holds_data_back_until_it_passes() {
    let backend = stalling_backend(PARTIAL).await;
    let proxy = proxy_with(backend, ", flush_interval = \"600ms\"");
    // The backend never finishes: the byte arrives because the interval passed.
    let elapsed = first_body_byte(proxy).await;
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
}

#[tokio::test]
async fn flush_interval_writes_complete_responses_at_once() {
    let backend = util::backend("ok").await;
    let proxy = proxy_with(backend, ", flush_interval = \"10s\"");
    let started = Instant::now();
    let (status, _, body) = util::get(proxy, "/").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"ok"[..]));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn flush_interval_skips_event_streams_and_unknown_lengths() {
    for response in [
        &b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nContent-Length: 9\r\n\r\ndata: a\n"[..],
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n",
    ] {
        let backend = stalling_backend(response).await;
        let proxy = proxy_with(backend, ", flush_interval = \"10s\"");
        let elapsed = first_body_byte(proxy).await;
        assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    }
}

/// Backend that switches protocols and then echoes.
async fn upgrading_echo_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::task::spawn(async move {
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
                    )
                    .await;
                while let Ok(read @ 1..) = stream.read(&mut buffer).await {
                    if stream.write_all(&buffer[..read]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    address
}

/// Opens an upgraded tunnel through `proxy`.
async fn open_tunnel(proxy: SocketAddr) -> TcpStream {
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
    assert!(echoes(&mut stream).await, "tunnel works before shutdown");
    stream
}

/// True if a byte sent through the tunnel comes back.
async fn echoes(stream: &mut TcpStream) -> bool {
    if stream.write_all(b"x").await.is_err() {
        return false;
    }
    let mut byte = [0; 1];
    matches!(
        tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte)).await,
        Ok(Ok(1))
    )
}

#[tokio::test]
async fn tunnels_close_stream_close_delay_after_shutdown() {
    let backend = upgrading_echo_backend().await;
    let (proxy, shutdown, _) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nstream_close_delay = \"500ms\"\nforward = \"{backend}\""
    ));
    let mut stream = open_tunnel(proxy).await;

    let started = Instant::now();
    shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(echoes(&mut stream).await, "still open during the delay");

    let mut byte = [0; 1];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
        .await
        .expect("tunnel should have been closed");
    assert!(!matches!(read, Ok(1..)), "{read:?}");
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(450), "{elapsed:?}");
}

#[tokio::test]
async fn zero_stream_close_delay_closes_tunnels_on_shutdown() {
    let backend = upgrading_echo_backend().await;
    let (proxy, shutdown, _) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nstream_close_delay = \"0s\"\nforward = \"{backend}\""
    ));
    let mut stream = open_tunnel(proxy).await;
    shutdown();
    let mut byte = [0; 1];
    let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
        .await
        .expect("tunnel should have been closed");
    assert!(!matches!(read, Ok(1..)), "{read:?}");
}

#[tokio::test]
async fn tunnels_outlive_shutdown_without_stream_close_delay() {
    let backend = upgrading_echo_backend().await;
    let (proxy, shutdown, _) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ));
    let mut stream = open_tunnel(proxy).await;
    shutdown();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(echoes(&mut stream).await, "tunnel kept open");
}
