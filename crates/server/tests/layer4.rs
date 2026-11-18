//! TCP and UDP (layer 4) proxying.

mod util;

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};

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
