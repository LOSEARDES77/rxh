//! `rate_limit` on servers and match entries, and `max_connections_per_ip`.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{HeaderMap, Request, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

/// `GET uri` with the given headers.
async fn get(proxy: SocketAddr, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap) {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let (status, headers, _) =
        util::send(proxy, request.body(Empty::<Bytes>::new()).unwrap()).await;
    (status, headers)
}

async fn status(proxy: SocketAddr, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
    get(proxy, uri, headers).await.0
}

#[tokio::test]
async fn requests_over_the_burst_get_429_and_never_reach_the_backend() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         rate_limit = {{ rate = 1, per = \"1h\", burst = 3 }}"
    ))[0];

    for _ in 0..3 {
        assert_eq!(status(proxy, "/", &[]).await, StatusCode::OK);
    }
    let (status, headers) = get(proxy, "/", &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = headers["retry-after"].to_str().unwrap().parse().unwrap();
    assert!(
        (3590..=3600).contains(&retry_after),
        "one token per hour, got Retry-After: {retry_after}"
    );
    assert_eq!(counters.requests(), 3);
}

#[tokio::test]
async fn tokens_come_back_at_the_rate() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         rate_limit = {{ rate = 1, per = \"300ms\" }}"
    ))[0];

    assert_eq!(status(proxy, "/", &[]).await, StatusCode::OK);
    let (status, headers) = get(proxy, "/", &[]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "1", "rounded up to a whole second");

    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(self::status(proxy, "/", &[]).await, StatusCode::OK);
    assert_eq!(
        self::status(proxy, "/", &[]).await,
        StatusCode::TOO_MANY_REQUESTS,
        "burst defaults to rate: one token at most"
    );
}

#[tokio::test]
async fn each_client_ip_of_a_trusted_proxy_has_its_own_bucket() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\ntrusted_proxies = [\"127.0.0.1\"]\n\
         forward = \"{backend}\"\nrate_limit = {{ rate = 1, per = \"1h\" }}"
    ))[0];
    let from = |client: &'static str| async move {
        status(proxy, "/", &[("x-forwarded-for", client)]).await
    };

    assert_eq!(from("10.0.0.1").await, StatusCode::OK);
    assert_eq!(from("10.0.0.1").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(from("10.0.0.2").await, StatusCode::OK);
    assert_eq!(
        status(proxy, "/", &[]).await,
        StatusCode::OK,
        "the peer itself"
    );
}

#[tokio::test]
async fn forwarded_for_from_untrusted_peers_does_not_pick_the_bucket() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         rate_limit = {{ rate = 1, per = \"1h\" }}"
    ))[0];

    let first = status(proxy, "/", &[("x-forwarded-for", "10.0.0.1")]).await;
    let second = status(proxy, "/", &[("x-forwarded-for", "10.0.0.2")]).await;
    assert_eq!(first, StatusCode::OK);
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn header_key_counts_per_header_value() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         rate_limit = {{ rate = 1, per = \"1h\", key = \"header:X-Api-Key\" }}"
    ))[0];
    let with_key =
        |key: &'static str| async move { status(proxy, "/", &[("x-api-key", key)]).await };

    assert_eq!(with_key("alice").await, StatusCode::OK);
    assert_eq!(with_key("alice").await, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(with_key("bob").await, StatusCode::OK);
    // Without the header, the client IP is the key.
    assert_eq!(status(proxy, "/", &[]).await, StatusCode::OK);
    assert_eq!(status(proxy, "/", &[]).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn route_limits_apply_on_top_of_the_server_limit() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nrate_limit = {{ rate = 4, per = \"1h\" }}\n\
         match = [\n\
         {{ uri = \"/login\", rate_limit = {{ rate = 1, per = \"1h\" }}, forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]"
    ))[0];

    assert_eq!(status(proxy, "/login", &[]).await, StatusCode::OK);
    assert_eq!(
        status(proxy, "/login", &[]).await,
        StatusCode::TOO_MANY_REQUESTS,
        "the route allows one request"
    );
    // Both /login requests took a server token, two are left.
    assert_eq!(status(proxy, "/", &[]).await, StatusCode::OK);
    assert_eq!(status(proxy, "/", &[]).await, StatusCode::OK);
    assert_eq!(status(proxy, "/", &[]).await, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn clients_rejected_by_ip_lists_do_not_use_tokens() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\ntrusted_proxies = [\"127.0.0.1\"]\n\
         forward = \"{backend}\"\nip_deny = [\"10.0.0.66\"]\n\
         rate_limit = {{ rate = 1, per = \"1h\", key = \"header:X-Api-Key\" }}"
    ))[0];
    let from = |client: &'static str| async move {
        let headers = [("x-forwarded-for", client), ("x-api-key", "shared")];
        status(proxy, "/", &headers).await
    };

    assert_eq!(from("10.0.0.66").await, StatusCode::FORBIDDEN);
    assert_eq!(from("10.0.0.1").await, StatusCode::OK);
    assert_eq!(
        from("10.0.0.66").await,
        StatusCode::FORBIDDEN,
        "403 wins over 429"
    );
}

/// Connects from `local` (any port) or the default address, sends `message`
/// and returns the open stream with what came back: empty when the proxy
/// closed the connection.
async fn exchange(proxy: SocketAddr, local: Option<&str>, message: &[u8]) -> (TcpStream, Vec<u8>) {
    let socket = TcpSocket::new_v4().unwrap();
    if let Some(local) = local {
        socket.bind(format!("{local}:0").parse().unwrap()).unwrap();
    }
    let mut stream = socket.connect(proxy).await.unwrap();
    // The proxy may already have closed a rejected connection.
    let _ = stream.write_all(message).await;
    let mut buffer = vec![0; 256];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("the proxy answers or closes")
        .unwrap_or(0);
    buffer.truncate(read);
    (stream, buffer)
}

/// Repeats `exchange` until the proxy answers: it notices a closed client
/// connection, and frees its slot, a moment after the client closes it.
async fn exchange_until_accepted(proxy: SocketAddr, message: &[u8]) -> (TcpStream, Vec<u8>) {
    for _ in 0..100 {
        let (stream, reply) = exchange(proxy, None, message).await;
        if !reply.is_empty() {
            return (stream, reply);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the slot of the closed connection was never freed");
}

#[tokio::test]
async fn tcp_closes_connections_over_the_limit_of_one_ip() {
    let echo = util::tcp_echo("echo:").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\n\
         max_connections_per_ip = 2\nforward = \"{echo}\""
    ))[0];

    let (first, reply) = exchange(proxy, None, b"one").await;
    assert_eq!(reply, b"echo:one");
    let (_second, reply) = exchange(proxy, None, b"two").await;
    assert_eq!(reply, b"echo:two");
    let (_, reply) = exchange(proxy, None, b"three").await;
    assert_eq!(reply, b"", "the third connection of 127.0.0.1 is closed");

    // Another client address has its own count.
    #[cfg(target_os = "linux")]
    {
        let (_other, reply) = exchange(proxy, Some("127.0.0.2"), b"other").await;
        assert_eq!(reply, b"echo:other");
    }

    drop(first);
    let (_third, reply) = exchange_until_accepted(proxy, b"again").await;
    assert_eq!(reply, b"echo:again");
}

#[tokio::test]
async fn http_closes_connections_over_the_limit_of_one_ip() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmax_connections_per_ip = 1\n\
         forward = \"{backend}\""
    ))[0];
    let request = b"GET / HTTP/1.1\r\nhost: test\r\n\r\n";

    let (first, reply) = exchange(proxy, None, request).await;
    assert!(reply.starts_with(b"HTTP/1.1 200"), "{reply:?}");
    // The first connection stays open (keep-alive) and holds the only slot.
    let (_, reply) = exchange(proxy, None, request).await;
    assert_eq!(reply, b"");

    drop(first);
    let (_second, reply) = exchange_until_accepted(proxy, request).await;
    assert!(reply.starts_with(b"HTTP/1.1 200"), "{reply:?}");
}
