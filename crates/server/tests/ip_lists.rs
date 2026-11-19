//! `ip_allow` and `ip_deny` on servers and match entries.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, StatusCode};
use rxh_test_util::free_address;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

/// `GET uri` with `X-Forwarded-For: client`, or none.
async fn status(proxy: SocketAddr, uri: &str, client: Option<&str>) -> StatusCode {
    let mut request = Request::get(uri);
    if let Some(client) = client {
        request = request.header("x-forwarded-for", client);
    }
    util::send(proxy, request.body(Empty::<Bytes>::new()).unwrap())
        .await
        .0
}

#[tokio::test]
async fn http_lists_use_the_client_ip_of_trusted_proxies() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\ntrusted_proxies = [\"127.0.0.1\"]\n\
         ip_allow = [\"10.0.0.0/8\"]\nip_deny = [\"10.0.0.66\"]\nmatch = [\n\
         {{ uri = \"/admin\", ip_allow = [\"10.1.0.0/16\"], ip_deny = [\"10.1.0.9\"], \
         forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]"
    ))[0];

    assert_eq!(status(proxy, "/", Some("10.2.3.4")).await, StatusCode::OK);
    assert_eq!(
        status(proxy, "/", Some("10.0.0.66")).await,
        StatusCode::FORBIDDEN,
        "server ip_deny wins over ip_allow"
    );
    assert_eq!(
        status(proxy, "/", Some("192.168.1.1")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(proxy, "/", None).await,
        StatusCode::FORBIDDEN,
        "without X-Forwarded-For the client is the peer, 127.0.0.1"
    );

    assert_eq!(
        status(proxy, "/admin", Some("10.1.0.5")).await,
        StatusCode::OK
    );
    assert_eq!(
        status(proxy, "/admin", Some("10.2.3.4")).await,
        StatusCode::FORBIDDEN,
        "a rejected entry answers 403 instead of falling through to the next one"
    );
    assert_eq!(
        status(proxy, "/admin", Some("10.1.0.9")).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn http_lists_ignore_forwarded_for_from_untrusted_peers() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nip_deny = [\"127.0.0.0/8\"]\n\
         forward = \"{backend}\""
    ))[0];

    assert_eq!(
        status(proxy, "/", Some("10.0.0.1")).await,
        StatusCode::FORBIDDEN,
        "an untrusted peer cannot pick its client IP"
    );
}

#[tokio::test]
async fn http_lists_apply_per_server_on_a_shared_listener() {
    let backend = util::backend("ok").await;
    let address = free_address();
    util::spawn(&format!(
        "[[server]]\nlisten = \"{address}\"\nhost = \"open.test\"\nforward = \"{backend}\"\n\
         [[server]]\nlisten = \"{address}\"\nhost = \"closed.test\"\n\
         ip_allow = [\"10.0.0.0/8\"]\nforward = \"{backend}\""
    ));

    let status = async |host: &str| {
        let request = Request::get("/").header("host", host);
        util::send(address, request.body(Empty::<Bytes>::new()).unwrap())
            .await
            .0
    };
    assert_eq!(status("open.test").await, StatusCode::OK);
    assert_eq!(status("closed.test").await, StatusCode::FORBIDDEN);
}

/// Sends `message` over TCP and returns what comes back before the
/// connection closes or a short wait ends.
async fn tcp_reply(proxy: SocketAddr, message: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy).await.unwrap();
    // The proxy may already have closed a rejected connection.
    let _ = stream.write_all(message).await;
    let mut buffer = vec![0; 64];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("the proxy answers or closes")
        .unwrap_or(0);
    buffer.truncate(read);
    buffer
}

#[tokio::test]
async fn tcp_lists_close_rejected_connections() {
    let echo = util::tcp_echo("echo:").await;
    let allowed = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\n\
         ip_allow = [\"127.0.0.0/8\"]\nip_deny = [\"127.0.0.2\"]\nforward = \"{echo}\""
    ))[0];
    let denied = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\n\
         ip_allow = [\"10.0.0.0/8\"]\nforward = \"{echo}\""
    ))[0];

    assert_eq!(tcp_reply(allowed, b"ping").await, b"echo:ping");
    assert_eq!(tcp_reply(denied, b"ping").await, b"");
}

/// Sends `message` over UDP and returns the reply, `None` after 500ms.
async fn udp_reply(proxy: SocketAddr, message: &[u8]) -> Option<Vec<u8>> {
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client.send_to(message, proxy).await.unwrap();
    let mut buffer = [0; 64];
    let (read, _) = tokio::time::timeout(Duration::from_millis(500), client.recv_from(&mut buffer))
        .await
        .ok()?
        .unwrap();
    Some(buffer[..read].to_vec())
}

#[tokio::test]
async fn udp_lists_drop_rejected_datagrams() {
    let echo = util::udp_echo("echo:").await;
    let allowed = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"udp\"\n\
         ip_allow = [\"127.0.0.1\"]\nforward = \"{echo}\""
    ))[0];
    let denied = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"udp\"\n\
         ip_deny = [\"127.0.0.0/8\"]\nforward = \"{echo}\""
    ))[0];

    assert_eq!(
        udp_reply(allowed, b"ping").await.as_deref(),
        Some(&b"echo:ping"[..])
    );
    assert_eq!(udp_reply(denied, b"ping").await, None);
}
