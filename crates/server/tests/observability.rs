//! Prometheus metrics on the admin API and JSON access logs.

mod util;

use std::fs;

use hyper::StatusCode;
use rxh_server::Daemon;
use rxh_test_util::free_address;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[tokio::test(flavor = "multi_thread")]
async fn metrics_and_json_access_log() {
    let web_backend = util::backend("hello").await;
    let echo = util::tcp_echo("e:").await;
    let (admin, web, raw) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let log_path = directory.path().join("access.log");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[admin]
listen = "{admin}"

[log]
format = "json"
output = '{log}'

[[server]]
name = "obs-web"
listen = "{web}"
[[server.match]]
uri = "/missing"
serve = "/nonexistent-rxh-dir"
[[server.match]]
forward = "{web_backend}"

[[server]]
name = "obs-raw"
listen = "{raw}"
protocol = "tcp"
forward = "{echo}"
"#,
            log = log_path.display()
        ),
    )
    .expect("write config");
    tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    for _ in 0..2 {
        let (status, _, body) = util::get(web, "/hello").await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"hello"[..]));
    }
    let (status, ..) = util::get(web, "/missing/file").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let mut stream = TcpStream::connect(raw).await.expect("connect");
    stream.write_all(b"hi").await.expect("write");
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await.expect("read");
    drop(stream);

    let (status, headers, body) = util::get(admin, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "text/plain; version=0.0.4");
    let text = String::from_utf8(body.to_vec()).expect("utf8");
    for expected in [
        "rxh_build_info{version=\"".to_owned(),
        "rxh_http_requests_total{server=\"obs-web\",code=\"200\"} 2".to_owned(),
        "rxh_http_requests_total{server=\"obs-web\",code=\"404\"} 1".to_owned(),
        "rxh_http_request_duration_seconds_count{server=\"obs-web\"} 3".to_owned(),
        "rxh_http_request_duration_seconds_bucket{server=\"obs-web\",le=\"+Inf\"} 3".to_owned(),
        "rxh_l4_connections_total{server=\"obs-raw\",protocol=\"tcp\"} 1".to_owned(),
        format!("rxh_upstream_requests_total{{pool=\"obs-web#1\",upstream=\"{web_backend}\"}} 2"),
        format!("rxh_upstream_failures_total{{pool=\"obs-web#1\",upstream=\"{web_backend}\"}} 0"),
        format!("rxh_upstream_healthy{{pool=\"obs-web#1\",upstream=\"{web_backend}\"}} 1"),
    ] {
        assert!(text.contains(&expected), "missing {expected} in\n{text}");
    }

    util::eventually(async || {
        fs::read_to_string(&log_path).is_ok_and(|log| log.lines().count() >= 4)
    })
    .await;
    let log = fs::read_to_string(&log_path).expect("log");
    let lines: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();

    let hello = lines
        .iter()
        .find(|line| line["uri"] == "/hello")
        .expect("proxied request logged");
    assert_eq!(hello["server"], "obs-web");
    assert_eq!(hello["method"], "GET");
    assert_eq!(hello["proto"], "HTTP/1.1");
    assert_eq!(hello["status"], 200);
    assert_eq!(hello["upstream"], web_backend.to_string());
    assert_eq!(hello["client_ip"], "127.0.0.1");
    assert!(
        hello["remote"]
            .as_str()
            .is_some_and(|remote| remote.starts_with("127.0.0.1:"))
    );
    assert!(hello["duration_ms"].is_number());
    assert!(
        hello["ts"]
            .as_str()
            .is_some_and(|ts| ts.ends_with('Z') && ts.len() == 24)
    );

    let missing = lines
        .iter()
        .find(|line| line["uri"] == "/missing/file")
        .expect("file request logged");
    assert_eq!(missing["status"], 404);
    assert!(missing["upstream"].is_null());

    let tcp = lines
        .iter()
        .find(|line| line["protocol"] == "tcp")
        .expect("tcp connection logged");
    assert_eq!(tcp["server"], "obs-raw");
    assert_eq!(tcp["upstream"], echo.to_string());
    assert_eq!(tcp["bytes_sent"], 2);
    assert_eq!(tcp["bytes_received"], 4);
    assert!(tcp["remote"].is_string() && tcp["duration_ms"].is_number() && tcp["ts"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn l4_connections_ending_any_way_write_one_json_record() {
    let echo = util::tcp_echo("e:").await;
    let udp = util::udp_echo("u:").await;
    let dead = util::dead_address();
    let (admin, idle, broken, datagrams) = (
        free_address(),
        free_address(),
        free_address(),
        free_address(),
    );
    let directory = tempfile::tempdir().expect("temp dir");
    let log_path = directory.path().join("l4.log");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            r#"
[admin]
listen = "{admin}"

[log]
format = "json"
output = '{log}'

[[server]]
name = "l4-idle"
listen = "{idle}"
protocol = "tcp"
forward = "{echo}"
idle_timeout = "200ms"

[[server]]
name = "l4-broken"
listen = "{broken}"
protocol = "tcp"
forward = "{dead}"

[[server]]
name = "l4-udp"
listen = "{datagrams}"
protocol = "udp"
forward = "{udp}"
udp_idle_timeout = "300ms"
"#,
            log = log_path.display()
        ),
    )
    .expect("write config");
    tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    // Idle timeout after some traffic: the proxy closes the connection and
    // still logs the bytes moved.
    let mut stream = TcpStream::connect(idle).await.expect("connect");
    stream.write_all(b"hi").await.expect("write");
    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await.expect("echo");
    let mut rest = Vec::new();
    stream
        .read_to_end(&mut rest)
        .await
        .expect("closed by idle timeout");

    // Failed dial.
    let mut stream = TcpStream::connect(broken).await.expect("connect");
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest).await;

    // UDP session that ends by idle timeout.
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind");
    socket.send_to(b"abc", datagrams).await.expect("send");
    let mut reply = [0; 16];
    let (len, _) = socket.recv_from(&mut reply).await.expect("reply");
    assert_eq!(&reply[..len], b"u:abc");

    let read = || -> Vec<serde_json::Value> {
        fs::read_to_string(&log_path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    };
    util::eventually(async || read().len() >= 3).await;
    let lines = read();
    assert_eq!(lines.len(), 3, "one record per connection: {lines:?}");

    let by_server = |name: &str| {
        lines
            .iter()
            .find(|line| line["server"] == name)
            .expect(name)
    };
    let idle_line = by_server("l4-idle");
    assert_eq!(idle_line["protocol"], "tcp");
    assert_eq!(idle_line["upstream"], echo.to_string());
    assert_eq!(idle_line["bytes_sent"], 2);
    assert_eq!(idle_line["bytes_received"], 4);
    let broken_line = by_server("l4-broken");
    assert!(broken_line["upstream"].is_null());
    assert_eq!(broken_line["protocol"], "tcp");
    let udp_line = by_server("l4-udp");
    assert_eq!(udp_line["protocol"], "udp");
    assert_eq!(udp_line["upstream"], udp.to_string());
    assert_eq!(udp_line["bytes_sent"], 3);
    assert_eq!(udp_line["bytes_received"], 5);

    let (_, _, body) = util::get(admin, "/metrics").await;
    let text = String::from_utf8(body.to_vec()).expect("utf8");
    for expected in [
        "rxh_l4_connections_total{server=\"l4-udp\",protocol=\"udp\"} 1",
        "rxh_l4_connections_total{server=\"l4-idle\",protocol=\"tcp\"} 1",
        "rxh_l4_connections_total{server=\"l4-broken\",protocol=\"tcp\"} 1",
    ] {
        assert!(text.contains(expected), "missing {expected} in\n{text}");
    }
}
