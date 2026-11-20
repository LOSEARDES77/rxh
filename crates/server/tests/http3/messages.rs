//! What reaches the backend from HTTP/3 requests: heads, bodies and their
//! declared lengths.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use rxh_test_util::{Fault, FlakyBackend};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::{Proxy, data_frame, forwarding, headers_frame, request_headers, util};

#[tokio::test(flavor = "multi_thread")]
async fn oversized_request_headers_are_rejected() {
    let (backend, requests) = util::echo_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    // Over the 16 KiB HTTP/2 listeners accept too.
    let answer = client.send_raw(&request_headers(20_000)).await;

    // Either a `431` head (a HEADERS frame, type 1) or a reset stream.
    if let Ok(answer) = &answer {
        assert_eq!(
            answer.first(),
            Some(&0x01),
            "not a HEADERS frame: {answer:?}"
        );
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the backend got the request"
    );
    let request = Request::get("https://localhost/")
        .body(Bytes::new())
        .unwrap();
    let response = client.send(request).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

/// Backend that reads one request head, reports it, never answers, and
/// reports when the proxy closes the connection.
async fn silent_backend() -> (SocketAddr, mpsc::Receiver<&'static str>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (events, receiver) = mpsc::channel(4);
    tokio::task::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                let _ = events.send("closed").await;
                return;
            }
            let head_before = received.windows(4).any(|window| window == b"\r\n\r\n");
            received.extend_from_slice(&buffer[..read]);
            if !head_before && received.windows(4).any(|window| window == b"\r\n\r\n") {
                let _ = events.send("head").await;
            }
        }
    });
    (address, receiver)
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reset_cancels_the_backend_request() {
    let (backend, mut events) = silent_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    // The head only: the request stream stays open, as an upload would.
    let request = Request::get("https://localhost/slow").body(()).unwrap();
    let open = client.open(request).await.unwrap();
    let head = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert_eq!(head.unwrap(), Some("head"));

    let reset = Instant::now();
    open.reset();
    let closed = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert_eq!(
        closed.expect("the backend request is cancelled within 2 s"),
        Some("closed")
    );
    assert!(reset.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn get_with_content_length_zero_is_not_chunked() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let request = Request::get("https://localhost/")
        .header("content-length", "0")
        .body(Bytes::new())
        .unwrap();
    let response = proxy
        .connect("localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    let seen = requests.recv().await.unwrap();
    assert!(
        !seen.headers.contains_key("transfer-encoding"),
        "sent upstream as {:?}",
        seen.headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_body_over_max_body_size_never_reaches_the_backend() {
    let (backend, requests) = util::echo_backend().await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "request = { max_body_size = \"1KB\" }"))
            .await;
    let mut client = proxy.connect("localhost").await;

    // The head alone: the length it declares must decide.
    let request = Request::post("https://localhost/")
        .header("content-length", "4000")
        .body(())
        .unwrap();
    let mut open = client.open(request).await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), open.status()).await;

    assert_eq!(
        status.expect("answered without the body").unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the backend got the request"
    );
}

// Current thread: the client queues the HEADERS frame and the end of the
// stream before its connection task runs, so both leave in one packet and
// the request has ended when the proxy reads its head.
#[tokio::test]
async fn get_ending_with_its_head_is_not_chunked() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let client = proxy.connect("localhost").await;

    client.send_raw(&request_headers(1)).await.unwrap();

    let seen = requests.recv().await.unwrap();
    assert!(
        !seen.headers.contains_key("transfer-encoding"),
        "sent upstream as {:?}",
        seen.headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_declaring_no_body_is_retried_before_its_stream_ends() {
    let flaky = FlakyBackend::spawn(Fault::AcceptThenClose).await;
    let good = util::backend("good").await;
    let proxy = Proxy::start(|site| {
        format!(
            "[upstream.pool]\nbackends = [\"{}\", \"{good}\"]\npolicy = \"round_robin\"\n\
             retries = 1\n\n\
             [[server]]\nlisten = \"{}\"\nforward = \"@pool\"\ntls = {}\n",
            flaky.address(),
            site.listen,
            site.tls
        )
    })
    .await;
    let mut client = proxy.connect("localhost").await;

    // Round robin sends one of the two to the backend that closes: only a
    // request known to have no body can be replayed on the other one. The
    // streams stay open, so `content-length: 0` is all that says so.
    for _ in 0..2 {
        let request = Request::get("https://localhost/")
            .header("content-length", "0")
            .body(())
            .unwrap();
        let mut open = client.open(request).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), open.status()).await;
        assert_eq!(status.unwrap().unwrap(), StatusCode::OK);
    }
    assert!(flaky.connections() >= 1, "the closing backend was tried");
}

/// Bytes each connection to a [`recording_backend`] carried, in order.
type Recorded = Arc<Mutex<Vec<Vec<u8>>>>;

/// Answer of a [`recording_backend`] to every request.
const OK: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";

/// Raw HTTP/1.1 backend that records the bytes of each connection. It reads
/// the `Content-Length` bytes after each request head as its body before
/// the next request, as a compliant server does, and answers `ok` to each
/// request: as soon as the head came if `early`, else once the body did.
async fn recording_backend(early: bool) -> (SocketAddr, Recorded) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = Recorded::default();
    let connections = Arc::clone(&recorded);
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let connections = Arc::clone(&connections);
            tokio::task::spawn(async move {
                let index = {
                    let mut connections = connections.lock().unwrap();
                    connections.push(Vec::new());
                    connections.len() - 1
                };
                let mut pending = Vec::new();
                // Body bytes still to read, while a body is being read.
                let mut body_left = None;
                let mut buffer = [0; 4096];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    connections.lock().unwrap()[index].extend_from_slice(&buffer[..read]);
                    pending.extend_from_slice(&buffer[..read]);
                    loop {
                        if let Some(left) = body_left {
                            let taken = pending.len().min(left);
                            pending.drain(..taken);
                            if taken < left {
                                body_left = Some(left - taken);
                                break;
                            }
                            body_left = None;
                            if !early {
                                let _ = stream.write_all(OK).await;
                            }
                            continue;
                        }
                        let Some(end) = pending.windows(4).position(|window| window == b"\r\n\r\n")
                        else {
                            break;
                        };
                        let head = String::from_utf8_lossy(&pending[..end]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse().unwrap_or(0));
                        pending.drain(..end + 4);
                        if early {
                            let _ = stream.write_all(OK).await;
                        }
                        body_left = Some(length);
                    }
                }
            });
        }
    });
    (address, recorded)
}

/// What followed the `POST /upload` head on each connection that carried
/// one.
fn after_post_heads(recorded: &Recorded) -> Vec<String> {
    let connections = recorded.lock().unwrap();
    let mut after = Vec::new();
    for bytes in connections.iter() {
        let text = String::from_utf8_lossy(bytes);
        if let Some(post) = text.find("POST /upload")
            && let Some(end) = text[post..].find("\r\n\r\n")
        {
            after.push(text[post + end + 4..].to_owned());
        }
    }
    after
}

/// `H3_MESSAGE_ERROR` as quinn prints a stream reset with it.
const MESSAGE_ERROR: &str = "error 270";

#[tokio::test(flavor = "multi_thread")]
async fn content_length_longer_than_the_body_is_not_forwarded_as_a_complete_request() {
    let (backend, recorded) = recording_backend(/* early= */ true).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let attacker = proxy.connect("localhost").await;

    // Five bytes announced, none sent: a backend reading the next request
    // on the same connection as this body would serve the attacker's
    // choice to the next client.
    let frames = headers_frame("POST", "/upload", &[("content-length", "5")]);
    let answer = tokio::time::timeout(Duration::from_secs(5), attacker.send_raw(&frames))
        .await
        .expect("answered within 5 s");
    let victim = proxy.get("/victim").await;

    let err = answer.expect_err("a request shorter than its Content-Length was answered");
    assert!(err.to_string().contains(MESSAGE_ERROR), "{err}");
    assert_eq!(victim.status, StatusCode::OK);
    assert_eq!(victim.body, "ok");
    for after in after_post_heads(&recorded) {
        assert!(
            after.len() < 5,
            "the backend got a whole body after the POST head: {after:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn content_length_shorter_than_the_body_is_rejected() {
    let (backend, recorded) = recording_backend(/* early= */ false).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let client = proxy.connect("localhost").await;

    // The extra bytes in the same DATA frame, and in a later one.
    let layouts: [&[&[u8]]; 2] = [&[b"1234567890"], &[b"12345", b"67890"]];
    for layout in layouts {
        let mut frames = headers_frame("POST", "/upload", &[("content-length", "5")]);
        for data in layout {
            frames.extend(data_frame(data));
        }
        let answer = tokio::time::timeout(Duration::from_secs(5), client.send_raw(&frames))
            .await
            .expect("answered within 5 s");

        let err = answer.expect_err("a body longer than its Content-Length was answered");
        assert!(err.to_string().contains(MESSAGE_ERROR), "{err}");
    }
    for after in after_post_heads(&recorded) {
        assert!(
            after.len() < 5,
            "the backend got a whole body after the POST head: {after:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_content_length_is_400() {
    let (backend, requests) = util::echo_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    let cases: [&[&str]; 5] = [&["+5"], &["5, 6"], &["5", "6"], &["abc"], &[""]];
    for values in cases {
        let mut request = Request::post("https://localhost/");
        for value in values {
            request = request.header("content-length", *value);
        }
        let request = request.body(Bytes::from("12345")).unwrap();
        let response = client.send(request).await.unwrap();

        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{values:?}");
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the backend got a request"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_identical_content_length_is_accepted() {
    let (backend, mut requests) = util::body_interceptor(200).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    let cases: [&[&str]; 2] = [&["5", "5"], &["5, 5"]];
    for values in cases {
        let mut request = Request::post("https://localhost/");
        for value in values {
            request = request.header("content-length", *value);
        }
        let request = request.body(Bytes::from("12345")).unwrap();
        let response = client.send(request).await.unwrap();

        assert_eq!(response.status, StatusCode::OK, "{values:?}");
        let (parts, body) = requests.recv().await.unwrap();
        let lengths: Vec<_> = parts.headers.get_all("content-length").iter().collect();
        assert_eq!(lengths, ["5"], "{values:?}");
        assert_eq!(body, "12345");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn not_modified_has_no_body() {
    let backend = util::backend_with(|request: Request<Incoming>| async move {
        let tag = "\"v1\"";
        let fresh = request
            .headers()
            .get("if-none-match")
            .is_some_and(|value| value == tag);
        let (status, body) = if fresh {
            (StatusCode::NOT_MODIFIED, "")
        } else {
            (StatusCode::OK, "representation")
        };
        Response::builder()
            .status(status)
            .header("etag", tag)
            .body(Full::new(Bytes::from(body)))
            .unwrap()
    })
    .await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let request = Request::get("https://localhost/")
        .header("if-none-match", "\"v1\"")
        .body(Bytes::new())
        .unwrap();
    let response = proxy
        .connect("localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::NOT_MODIFIED);
    assert_eq!(response.headers["etag"], "\"v1\"");
    assert!(response.body.is_empty(), "304 answered {:?}", response.body);
}
