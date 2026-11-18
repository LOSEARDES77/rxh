//! HTTP reverse proxy behaviour: forwarding, headers, upgrades, cache, h2.

mod util;

use std::sync::Arc;
use std::sync::atomic::AtomicU16;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header;
use hyper::{Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test]
async fn forwards_requests_to_the_backend() {
    let backend = util::backend("Hello world").await;
    let proxy = util::spawn(&format!(
        "[admin]\nenabled = false\n[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ))[0];

    let (status, headers, body) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Hello world");
    assert!(
        headers[header::SERVER]
            .to_str()
            .unwrap()
            .starts_with("rxh/")
    );
}

#[tokio::test]
async fn unknown_prefix_is_404() {
    let backend = util::backend("unused").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nuri = \"/prefix\"\nforward = \"{backend}\""
    ))[0];

    for uri in ["/unknown", "/invalid", "/test/longer"] {
        assert_eq!(util::get(proxy, uri).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(util::get(proxy, "/prefix/x").await.0, StatusCode::OK);
}

#[tokio::test]
async fn dead_backend_is_502_and_empty_pool_is_503() {
    let dead = util::dead_address();
    let proxies = util::spawn(&format!(
        r#"
        [upstream.empty]
        backends = []

        [[server]]
        listen = "127.0.0.1:0"
        forward = "{dead}"

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@empty"
        "#
    ));

    assert_eq!(util::get(proxies[0], "/").await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(
        util::get(proxies[1], "/").await.0,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn forwarding_headers_follow_rfc_7239_and_caddy() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        r#"
        [[server]]
        listen = "127.0.0.1:0"
        name = "edge-proxy"
        forward = "{backend}"
        request.headers.set = {{ "x-added" = "ok" }}
        request.headers.remove = ["x-remove-me"]
        response.headers.via = {{ extend = true }}
        response.headers.server = {{ version = false }}
        response.headers.set = {{ "x-response-added" = "yes" }}
        response.headers.remove = ["x-response-remove"]
        "#
    ))[0];

    let request = Request::get("/path?q=1")
        .header(header::HOST, "example.com")
        .header("x-remove-me", "delete")
        .header("x-forwarded-for", "6.6.6.6")
        .header(header::CONNECTION, "keep-alive, x-hop")
        .header("x-hop", "secret")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let (status, headers, _) = util::send(proxy, request).await;
    let upstream = requests.recv().await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.uri, "/path?q=1");
    assert_eq!(upstream.headers[header::HOST], "example.com");
    assert_eq!(upstream.headers["x-added"], "ok");
    assert!(!upstream.headers.contains_key("x-remove-me"));
    assert!(
        !upstream.headers.contains_key("x-hop"),
        "hop-by-hop headers must not leak"
    );
    assert_eq!(
        upstream.headers["x-forwarded-for"], "127.0.0.1",
        "untrusted XFF is replaced"
    );
    assert_eq!(upstream.headers["x-forwarded-proto"], "http");
    assert_eq!(upstream.headers["x-forwarded-host"], "example.com");
    let forwarded = upstream.headers[header::FORWARDED].to_str().unwrap();
    assert!(forwarded.starts_with("for=\"127.0.0.1:"), "{forwarded}");
    assert!(
        forwarded.ends_with(";by=edge-proxy;host=example.com;proto=http"),
        "{forwarded}"
    );
    assert_eq!(upstream.headers[header::VIA], "1.1 edge-proxy");

    assert_eq!(headers[header::SERVER], "rxh");
    assert_eq!(headers[header::VIA], "1.1 edge-proxy");
    assert_eq!(headers["x-response-added"], "yes");
    assert!(!headers.contains_key("x-response-remove"));
}

#[tokio::test]
async fn upstream_connections_are_kept_alive() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ))[0];

    for _ in 0..20 {
        assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
    }

    assert_eq!(counters.requests(), 20);
    assert_eq!(
        counters.connections(),
        1,
        "sequential requests must reuse one connection"
    );
}

#[tokio::test]
async fn upgraded_connections_are_tunneled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend = listener.local_addr().unwrap();
    tokio::task::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = [0; 4096];
        let read = stream.read(&mut head).await.unwrap();
        let request = String::from_utf8_lossy(&head[..read]).to_ascii_lowercase();
        assert!(request.contains("upgrade: testproto"), "{request}");
        stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
            )
            .await
            .unwrap();
        let mut buffer = [0; 1024];
        let read = stream.read(&mut buffer).await.unwrap();
        stream.write_all(&buffer[..read]).await.unwrap();
    });

    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ))[0];
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\nUpgrade: testproto\r\n\r\n",
        )
        .await
        .unwrap();

    let mut head = [0; 4096];
    let read = stream.read(&mut head).await.unwrap();
    let response = String::from_utf8_lossy(&head[..read]).to_ascii_lowercase();
    assert!(response.starts_with("http/1.1 101"), "{response}");
    assert!(response.contains("upgrade: testproto"));

    stream.write_all(b"Test String").await.unwrap();
    let mut buffer = [0; 1024];
    let read = stream.read(&mut buffer).await.unwrap();
    assert_eq!(&buffer[..read], b"Test String");
}

#[tokio::test]
async fn cache_serves_repeated_gets() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\ncache = {{ enabled = true, ttl_seconds = 60 }}"
    ))[0];

    let (_, _, first) = util::get(proxy, "/cached").await;
    let (_, _, second) = util::get(proxy, "/cached").await;
    util::get(proxy, "/other").await;

    assert_eq!(first, second);
    assert_eq!(counters.requests(), 2);
}

#[tokio::test]
async fn http2_clients_reach_http1_backends() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ))[0];

    let stream = TcpStream::connect(proxy).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);

    let request = Request::get("http://example.com:8080/h2?x=1")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "intercepted"
    );

    let upstream = requests.recv().await.unwrap();
    assert_eq!(upstream.version, http::Version::HTTP_11);
    assert_eq!(upstream.uri, "/h2?x=1");
    assert_eq!(upstream.headers[header::HOST], "example.com:8080");
}

#[tokio::test]
async fn routes_by_host_method_and_header() {
    let a = util::backend("a").await;
    let b = util::backend("b").await;
    let c = util::backend("c").await;
    let d = util::backend("d").await;
    let address = util::spawn(&format!(
        r#"
[[server]]
listen = "127.0.0.1:0"
match = [
    {{ host = "a.test", forward = "{a}" }},
    {{ host = "*.b.test", forward = "{b}" }},
    {{ host = "c.test", method = "POST", forward = "{c}" }},
    {{ host = "c.test", header = {{ x-beta = "*" }}, forward = "{d}" }},
]
"#
    ))[0];

    let send = async |method: &str, host: &str, extra: &[(&str, &str)]| {
        let mut request = Request::builder()
            .method(method)
            .uri("/")
            .header("host", host);
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        let (status, _, body) = util::send(address, request.body(Empty::new()).unwrap()).await;
        (status, String::from_utf8(body.to_vec()).unwrap())
    };

    assert_eq!(send("GET", "a.test", &[]).await.1, "a");
    assert_eq!(send("GET", "A.TEST:8080", &[]).await.1, "a");
    assert_eq!(send("GET", "x.b.test", &[]).await.1, "b");
    assert_eq!(send("POST", "c.test", &[]).await.1, "c");
    assert_eq!(send("GET", "c.test", &[("x-beta", "1")]).await.1, "d");
    assert_eq!(send("GET", "c.test", &[]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        send("GET", "unknown.test", &[]).await.0,
        StatusCode::NOT_FOUND
    );
}
