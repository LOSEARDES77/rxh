//! `max_conns_per_host`, `request_buffers`, `response_buffers` and
//! `request.max_body_size`.

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hyper::StatusCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Connections a [`slow_backend`] has open now, had open at most, and accepted.
#[derive(Default)]
struct Connections {
    open: AtomicUsize,
    peak: AtomicUsize,
    accepted: AtomicUsize,
}

/// Reads one request head. `false` when the peer closed the connection.
async fn read_head(stream: &mut TcpStream) -> bool {
    let mut head = Vec::new();
    let mut byte = [0; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if !matches!(stream.read(&mut byte).await, Ok(1)) {
            return false;
        }
        head.push(byte[0]);
    }
    true
}

/// Keep-alive backend that takes 100ms per request and answers
/// `101 Switching Protocols` to `/tunnel`.
async fn slow_backend() -> (SocketAddr, Arc<Connections>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    let connections = Arc::new(Connections::default());
    let shared = Arc::clone(&connections);
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let connections = Arc::clone(&shared);
            connections.accepted.fetch_add(1, Ordering::SeqCst);
            let open = connections.open.fetch_add(1, Ordering::SeqCst) + 1;
            connections.peak.fetch_max(open, Ordering::SeqCst);
            tokio::task::spawn(async move {
                while read_head(&mut stream).await {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let reply = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
                    if stream.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
                connections.open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    (address, connections)
}

/// Six concurrent requests through a proxy whose pool has `options`.
async fn six_requests(options: &str) -> (Arc<Connections>, Duration) {
    let (backend, connections) = slow_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"]{options} }}"
    ))[0];

    let started = Instant::now();
    let mut requests = Vec::new();
    for _ in 0..6 {
        requests.push(tokio::task::spawn(util::get(proxy, "/")));
    }
    for request in requests {
        let (status, _, body) = request.await.expect("request task");
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"ok"[..]));
    }
    (connections, started.elapsed())
}

#[tokio::test]
async fn max_conns_per_host_caps_backend_connections() {
    let (connections, elapsed) = six_requests(", max_conns_per_host = 2").await;
    assert_eq!(connections.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        connections.accepted.load(Ordering::SeqCst),
        2,
        "waiting requests reuse the two connections"
    );
    assert!(
        elapsed >= Duration::from_millis(280),
        "six 100ms requests, two at a time, took {elapsed:?}"
    );

    // Without the limit every request gets its own connection at once.
    let (connections, _) = six_requests("").await;
    assert_eq!(connections.peak.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn max_conns_per_host_holds_without_keepalive() {
    let (connections, _) = six_requests(", max_conns_per_host = 1, keepalive = \"0s\"").await;
    assert_eq!(connections.peak.load(Ordering::SeqCst), 1);
    assert_eq!(connections.accepted.load(Ordering::SeqCst), 6);
}

#[tokio::test]
async fn max_conns_per_host_counts_upgraded_tunnels() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let backend = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        // First connection: a tunnel held until the client closes it.
        let (mut tunnel, _) = listener.accept().await.expect("accept");
        assert!(read_head(&mut tunnel).await);
        tunnel
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
            )
            .await
            .expect("101");
        let mut rest = Vec::new();
        let _ = tunnel.read_to_end(&mut rest).await;
        drop(tunnel);

        let (mut stream, _) = listener.accept().await.expect("accept");
        assert!(read_head(&mut stream).await);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nafter")
            .await
            .expect("200");
    });
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"], max_conns_per_host = 1 }}"
    ))[0];

    let mut tunnel = TcpStream::connect(proxy).await.expect("connect");
    tunnel
        .write_all(
            b"GET / HTTP/1.1\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
        )
        .await
        .expect("upgrade request");
    assert!(read_head(&mut tunnel).await, "101 from the proxy");

    let mut waiting = tokio::task::spawn(util::get(proxy, "/"));
    let early = tokio::time::timeout(Duration::from_millis(300), &mut waiting).await;
    assert!(early.is_err(), "the tunnel holds the only connection");

    drop(tunnel);
    let (status, _, body) = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .expect("served once the tunnel closed")
        .expect("request task");
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"after"[..]));
}

/// Sends `request` (which must ask for `Connection: close`) and returns the
/// status and everything after the status line, lower cased headers included.
async fn raw(proxy: SocketAddr, request: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.write_all(request).await.expect("request");
    finish(stream).await
}

/// Reads the response of [`raw`].
async fn finish(mut stream: TcpStream) -> (u16, String) {
    let mut response = Vec::new();
    // A reset after the response is fine: the proxy may close without
    // reading a refused body.
    let _ = stream.read_to_end(&mut response).await;
    let response = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = response.split_once("\r\n\r\n").expect("response head");
    let status = head
        .split(' ')
        .nth(1)
        .expect("status")
        .parse()
        .expect("status");
    (
        status,
        format!("{}\r\n\r\n{body}", head.to_ascii_lowercase()),
    )
}

/// `POST /` with a `Content-Length` body.
fn post(body: &str) -> Vec<u8> {
    post_to("/", body)
}

/// `POST` to `uri` with a `Content-Length` body.
fn post_to(uri: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {uri} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// `POST` with a chunked body, one chunk per element.
fn chunked(chunks: &[&str]) -> Vec<u8> {
    let mut request = String::from(
        "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
    for chunk in chunks {
        request.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
    }
    request.push_str("0\r\n\r\n");
    request.into_bytes()
}

/// Proxy in front of an echo backend, with `pool` and `server` options.
async fn echo_proxy(pool: &str, server: &str) -> (SocketAddr, Arc<AtomicUsize>) {
    let (backend, requests) = util::echo_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\n{server}\nforward = {{ backends = [\"{backend}\"]{pool} }}"
    ))[0];
    (proxy, requests)
}

#[tokio::test]
async fn request_buffers_hold_the_body_before_dialing() {
    let (proxy, requests) = echo_proxy(", request_buffers = \"1KiB\"", "").await;

    let request = chunked(&["hello", " world"]);
    let (first, second) = request.split_at(request.len() - 5);
    let mut stream = TcpStream::connect(proxy).await.expect("connect");
    stream.write_all(first).await.expect("head and chunks");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "nothing goes upstream before the body is complete"
    );
    stream.write_all(second).await.expect("last chunk");

    let (status, response) = finish(stream).await;
    assert_eq!(status, 200);
    assert!(response.ends_with("\r\n\r\nhello world"), "{response}");
    assert!(
        response.contains("x-request-length: 11\r\n"),
        "a buffered body is sent with its length: {response}"
    );
}

#[tokio::test]
async fn request_bodies_over_the_buffer_are_streamed_whole() {
    let (proxy, _) = echo_proxy(", request_buffers = 1024", "").await;
    let part = "0123456789abcdef".repeat(1024);

    let (status, response) = raw(proxy, &chunked(&[&part, &part, "end"])).await;
    assert_eq!(status, 200);
    assert!(response.contains("x-request-length: none\r\n"), "streamed");
    assert!(response.ends_with(&format!("{part}{part}end")));

    // Without buffering a chunked body stays chunked.
    let (proxy, _) = echo_proxy("", "").await;
    let (_, response) = raw(proxy, &chunked(&["hello"])).await;
    assert!(
        response.contains("x-request-length: none\r\n"),
        "{response}"
    );
}

/// Backend answering with a chunked `hello world` in two parts, 100ms
/// apart. With `complete = false` it closes instead of sending the end.
async fn chunked_backend(complete: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::task::spawn(async move {
                if !read_head(&mut stream).await {
                    return;
                }
                let head = "HTTP/1.1 200 OK\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n";
                let _ = stream.write_all(head.as_bytes()).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                if complete {
                    let _ = stream.write_all(b"6\r\n world\r\n0\r\n\r\n").await;
                }
            });
        }
    });
    address
}

const GET: &[u8] = b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

#[tokio::test]
async fn response_buffers_hold_the_body_before_answering() {
    let proxy_with = |backend: SocketAddr, options: &str| {
        util::spawn(&format!(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"]{options} }}"
        ))[0]
    };
    let backend = chunked_backend(/* complete= */ true).await;

    let (status, response) = raw(proxy_with(backend, ", response_buffers = \"1KiB\""), GET).await;
    assert_eq!(status, 200);
    assert!(
        response.contains("content-length: 11\r\n") && !response.contains("chunked"),
        "a buffered response is sent with its length: {response}"
    );
    assert!(response.ends_with("\r\n\r\nhello world"), "{response}");

    // Larger than the buffer: the rest is streamed.
    let (status, response) = raw(proxy_with(backend, ", response_buffers = 4"), GET).await;
    assert_eq!(status, 200);
    assert!(
        response.contains("transfer-encoding: chunked"),
        "{response}"
    );
    assert!(response.contains("hello") && response.contains(" world"));

    // Not buffered: streamed as it comes.
    let (_, response) = raw(proxy_with(backend, ""), GET).await;
    assert!(
        response.contains("transfer-encoding: chunked"),
        "{response}"
    );
}

#[tokio::test]
async fn response_cut_short_while_buffering_is_a_502() {
    let backend = chunked_backend(/* complete= */ false).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"], response_buffers = 1024 }}"
    ))[0];

    let (status, response) = raw(proxy, GET).await;
    assert_eq!(status, 502, "{response}");
}

#[tokio::test]
async fn declared_body_over_the_limit_gets_a_413() {
    let (proxy, requests) = echo_proxy("", "request = { max_body_size = 10 }").await;

    let (status, response) = raw(proxy, &post("0123456789")).await;
    assert_eq!(status, 200, "a body of exactly the limit passes");
    assert!(response.ends_with("\r\n\r\n0123456789"), "{response}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    let (status, _) = raw(proxy, &post("0123456789a")).await;
    assert_eq!(status, 413);
    assert_eq!(requests.load(Ordering::SeqCst), 1, "refused before dialing");
}

#[tokio::test]
async fn chunked_body_over_the_limit_gets_a_413() {
    let (proxy, _) = echo_proxy("", "request = { max_body_size = \"1KiB\" }").await;
    let part = "x".repeat(600);

    let (status, response) = raw(proxy, &chunked(&[&part])).await;
    assert_eq!(status, 200);
    assert!(response.ends_with(&part));

    let (status, response) = raw(proxy, &chunked(&[&part, &part])).await;
    assert_eq!(status, 413, "{response}");

    // The backend is not blamed: it still serves.
    let (status, _) = raw(proxy, &post("ok")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn buffered_body_over_the_limit_never_reaches_the_backend() {
    let (proxy, requests) = echo_proxy(
        ", request_buffers = \"1MiB\"",
        "request = { max_body_size = 1000 }",
    )
    .await;
    let part = "x".repeat(600);

    let (status, _) = raw(proxy, &chunked(&[&part, &part])).await;
    assert_eq!(status, 413);
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn body_limit_applies_to_redirect_routes_and_each_match_entry() {
    let (backend, _) = util::echo_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmatch = [\n\
         {{ uri = \"/moved\", redirect = \"/new\", request = {{ max_body_size = 4 }} }},\n\
         {{ uri = \"/\", forward = \"{backend}\" }},\n]"
    ))[0];
    assert_eq!(raw(proxy, &post_to("/moved", "12345")).await.0, 413);
    assert_eq!(raw(proxy, &post_to("/moved", "1234")).await.0, 302);
    assert_eq!(raw(proxy, &post("no limit on this route")).await.0, 200);
}
