//! TCP and UDP (layer 4) proxying, layer 4 routing and PROXY protocol input.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::Request;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

async fn tcp_round_trip(proxy: std::net::SocketAddr, message: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy).await.expect("proxy accepts");
    stream.write_all(message).await.expect("write");
    let mut buffer = vec![0; 1024];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
        .await
        .expect("echo answers")
        .expect("read");
    buffer.truncate(read);
    buffer
}

#[tokio::test]
async fn tcp_round_robin_across_backends() {
    let one = util::tcp_echo("one:").await;
    let two = util::tcp_echo("two:").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = {{ policy = \"round_robin\", backends = [\"{one}\", \"{two}\"] }}"
    ))[0];

    let replies = [
        tcp_round_trip(proxy, b"ping").await,
        tcp_round_trip(proxy, b"ping").await,
        tcp_round_trip(proxy, b"ping").await,
    ];
    assert_eq!(replies[0], b"one:ping");
    assert_eq!(replies[1], b"two:ping");
    assert_eq!(replies[2], b"one:ping");
}

#[tokio::test]
async fn tcp_retries_dead_backend_and_ip_hash_sticks() {
    let alive = util::tcp_echo("alive:").await;
    let dead = util::dead_address();
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = {{ policy = \"ip_hash\", retries = 1, try_interval = \"1ms\", passive = {{ fail_duration = \"1m\" }}, backends = [\"{dead}\", \"{alive}\"] }}"
    ))[0];

    for _ in 0..5 {
        assert_eq!(tcp_round_trip(proxy, b"x").await, b"alive:x");
    }
}

#[tokio::test]
async fn tcp_sends_proxy_protocol_header() {
    let (backend, mut captured) = util::tcp_capture(16).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nproxy_protocol = \"v2\"\nforward = \"{backend}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream.write_all(b"payload").await.unwrap();
    let header = captured.recv().await.unwrap();
    assert_eq!(&header[..12], b"\r\n\r\n\0\r\nQUIT\n");
    assert_eq!(header[13], 0x11, "TCP over IPv4");
}

#[tokio::test]
async fn udp_sessions_are_balanced_and_sticky() {
    let one = util::udp_echo("one:").await;
    let two = util::udp_echo("two:").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"udp\"\nudp_idle_timeout = \"2s\"\nforward = {{ policy = \"round_robin\", backends = [\"{one}\", \"{two}\"] }}"
    ))[0];

    let mut tags = Vec::new();
    for _ in 0..2 {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut seen = Vec::new();
        for round in 0..3 {
            client
                .send_to(format!("m{round}").as_bytes(), proxy)
                .await
                .unwrap();
            let mut buffer = [0; 128];
            let (read, _) =
                tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buffer))
                    .await
                    .expect("UDP echo answers")
                    .unwrap();
            let reply = String::from_utf8_lossy(&buffer[..read]).into_owned();
            assert!(reply.ends_with(&format!("m{round}")), "{reply}");
            seen.push(reply.split(':').next().unwrap().to_owned());
        }
        assert!(
            seen.iter().all(|tag| tag == &seen[0]),
            "a client session must stick: {seen:?}"
        );
        tags.push(seen[0].clone());
    }
    assert_ne!(
        tags[0], tags[1],
        "two clients should land on different backends"
    );
}

/// TLS server with a fresh certificate for `name` that writes `name` once
/// the handshake is done. Returns its address and certificate.
async fn tls_backend(name: &'static str) -> (SocketAddr, CertificateDer<'static>) {
    let certificate = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
    let der = CertificateDer::from(certificate.cert.der().to_vec());
    let key = PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::task::spawn(async move {
                if let Ok(mut stream) = acceptor.accept(stream).await {
                    let _ = stream.write_all(name.as_bytes()).await;
                    let _ = stream.flush().await;
                    let _ = stream.read(&mut [0; 16]).await;
                }
            });
        }
    });
    (address, der)
}

#[tokio::test]
async fn tcp_routes_tls_passthrough_by_sni() {
    let (a, a_cert) = tls_backend("a.test").await;
    let (b, b_cert) = tls_backend("b.test").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nmatch = [\n\
         {{ sni = \"a.test\", forward = \"{a}\" }},\n\
         {{ sni = \"*.b.test\", forward = \"{b}\" }},\n\
         {{ sni = \"b.test\", alpn = \"h2\", forward = \"{b}\" }},\n]"
    ))[0];

    let mut roots = RootCertStore::empty();
    roots.add(a_cert.clone()).unwrap();
    roots.add(b_cert.clone()).unwrap();
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(Arc::new(config));

    let connect = |name: &'static str| {
        let connector = connector.clone();
        async move {
            let stream = TcpStream::connect(proxy).await.unwrap();
            let name = ServerName::try_from(name).unwrap();
            tokio::time::timeout(Duration::from_secs(5), connector.connect(name, stream))
                .await
                .expect("handshake finishes")
        }
    };

    for (name, certificate) in [("a.test", &a_cert), ("b.test", &b_cert)] {
        let mut stream = connect(name).await.expect("handshake through the proxy");
        let received = &stream.get_ref().1.peer_certificates().unwrap()[0];
        assert_eq!(received, certificate, "{name} reaches its own backend");
        let mut tag = vec![0; name.len()];
        stream.read_exact(&mut tag).await.unwrap();
        assert_eq!(tag, name.as_bytes());
    }

    // No entry matches: the proxy closes the connection.
    assert!(connect("c.test").await.is_err());
}

#[tokio::test]
async fn tcp_routes_http_by_host() {
    let a = util::backend("site a").await;
    let b = util::backend("site b").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nmatch = [\n\
         {{ protocol = \"ssh\", forward = \"{}\" }},\n\
         {{ protocol = \"http\", http_host = \"a.test\", forward = \"{a}\" }},\n\
         {{ forward = \"{b}\" }},\n]",
        util::dead_address()
    ))[0];

    for (host, body) in [
        ("a.test", "site a"),
        ("A.TEST:80", "site a"),
        ("b.test", "site b"),
    ] {
        let request = Request::get("/")
            .header("host", host)
            .body(Empty::<Bytes>::new())
            .unwrap();
        let (_, _, received) = util::send(proxy, request).await;
        assert_eq!(received, body, "Host {host}");
    }
}

/// Opens a connection to `address` that starts with `preamble`, then sends
/// `GET /` and returns the response status.
async fn get_after(address: SocketAddr, preamble: &[u8]) -> Option<hyper::StatusCode> {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(preamble).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::task::spawn(connection);
    let request = Request::get("/")
        .header("host", "x.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), sender.send_request(request))
        .await
        .expect("answer or close in time");
    response.ok().map(|response| response.status())
}

#[tokio::test]
async fn http_server_takes_client_address_from_proxy_protocol() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol_in = true\nforward = \"{backend}\""
    ))[0];

    let status = get_after(proxy, b"PROXY TCP4 203.0.113.7 127.0.0.1 40000 80\r\n").await;
    assert_eq!(status, Some(hyper::StatusCode::OK));
    let parts = requests.recv().await.unwrap();
    assert_eq!(parts.headers["x-forwarded-for"], "203.0.113.7");

    // v2 LOCAL keeps the real peer.
    let local = b"\r\n\r\n\0\r\nQUIT\n\x20\x00\x00\x00";
    assert_eq!(get_after(proxy, local).await, Some(hyper::StatusCode::OK));
    let parts = requests.recv().await.unwrap();
    assert_eq!(parts.headers["x-forwarded-for"], "127.0.0.1");

    // Without a header the connection is closed.
    assert_eq!(get_after(proxy, b"").await, None);
}

#[tokio::test]
async fn proxy_protocol_in_rejects_untrusted_peers() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol_in = true\n\
         trusted_proxies = [\"10.0.0.0/8\"]\nforward = \"{backend}\""
    ))[0];
    let header = b"PROXY TCP4 203.0.113.7 127.0.0.1 40000 80\r\n";
    assert_eq!(get_after(proxy, header).await, None);
}

#[tokio::test]
async fn tcp_forwards_proxy_protocol_source_upstream() {
    let (backend, mut captured) = util::tcp_capture(23).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nproxy_protocol_in = true\n\
         proxy_protocol = \"v1\"\nforward = \"{backend}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.unwrap();
    let mut header = b"\r\n\r\n\0\r\nQUIT\n\x21\x11\x00\x0c".to_vec();
    header.extend_from_slice(&[203, 0, 113, 7, 127, 0, 0, 1, 0x9c, 0x40, 0, 80]);
    header.extend_from_slice(b"payload");
    stream.write_all(&header).await.unwrap();
    let received = captured.recv().await.unwrap();
    assert_eq!(received, b"PROXY TCP4 203.0.113.7 ");
}

#[tokio::test]
async fn tcp_with_tls_termination_routes_by_handshake_sni() {
    let dir = tempfile::tempdir().unwrap();
    let certificate =
        rcgen::generate_simple_self_signed(vec![String::from("a.test"), String::from("b.test")])
            .unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, certificate.cert.pem()).unwrap();
    std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();

    let a = util::tcp_echo("a:").await;
    let b = util::tcp_echo("b:").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\n\
         tls = {{ cert = {cert:?}, key = {key:?} }}\nmatch = [\n\
         {{ sni = \"a.test\", forward = \"{a}\" }},\n\
         {{ forward = \"{b}\" }},\n]"
    ))[0];

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(certificate.cert.der().to_vec()))
        .unwrap();
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    for (name, reply) in [("a.test", &b"a:ping"[..]), ("b.test", b"b:ping")] {
        let stream = TcpStream::connect(proxy).await.unwrap();
        let mut stream = connector
            .connect(ServerName::try_from(name).unwrap(), stream)
            .await
            .unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut received = vec![0; reply.len()];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut received))
            .await
            .expect("echo answers")
            .unwrap();
        assert_eq!(received, reply, "{name}");
    }
}
