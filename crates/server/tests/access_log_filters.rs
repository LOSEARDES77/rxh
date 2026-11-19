//! Access log filters (`status`, `min_duration`) and sampling.

mod util;

use std::fs;
use std::path::Path;
use std::time::Duration;

use http_body_util::Full;
use hyper::{Response, StatusCode};
use rxh_server::Daemon;
use rxh_test_util::free_address;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Starts a daemon with `config`, waiting for its admin API on `admin`.
async fn start(directory: &Path, admin: std::net::SocketAddr, config: &str) {
    let config_path = directory.join("rxh.toml");
    fs::write(&config_path, config).expect("write config");
    tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;
}

fn read(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn only_lines_matching_a_filter_are_written() {
    let fast = util::backend("fast").await;
    let slow = util::backend_with(|_| async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        Response::new(Full::from("slow"))
    })
    .await;
    let echo = util::tcp_echo("e:").await;
    let (admin, web, raw) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let log_path = directory.path().join("access.log");
    start(
        directory.path(),
        admin,
        &format!(
            r#"
[admin]
listen = "{admin}"

[log]
format = "json"
output = '{log}'
status = ["4xx"]
min_duration = "300ms"

[[server]]
name = "web"
listen = "{web}"
[[server.match]]
uri = "/slow"
forward = "{slow}"
[[server.match]]
uri = "/missing"
serve = "/nonexistent-rxh-dir"
[[server.match]]
forward = "{fast}"

[[server]]
name = "raw"
listen = "{raw}"
protocol = "tcp"
forward = "{echo}"
"#,
            log = log_path.display()
        ),
    )
    .await;

    // A short TCP connection is dropped, a long one is kept.
    for (pause, payload) in [(0, &b"short"[..]), (400, &b"long"[..])] {
        let mut stream = TcpStream::connect(raw).await.expect("connect");
        stream.write_all(payload).await.expect("write");
        let mut reply = vec![0; payload.len() + 2];
        stream.read_exact(&mut reply).await.expect("echo");
        tokio::time::sleep(Duration::from_millis(pause)).await;
        drop(stream);
    }
    let (status, ..) = util::get(web, "/fast").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, body) = util::get(web, "/slow").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"slow"[..]));
    let (status, ..) = util::get(web, "/missing/file").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    util::eventually(async || read(&log_path).len() >= 3).await;
    // Room for a line that should not be there.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let lines = read(&log_path);
    assert_eq!(lines.len(), 3, "{lines:?}");
    let uris: Vec<&str> = lines
        .iter()
        .filter_map(|line| line["uri"].as_str())
        .collect();
    assert_eq!(uris, ["/slow", "/missing/file"]);
    let tcp: Vec<&serde_json::Value> = lines
        .iter()
        .filter(|line| line["protocol"] == "tcp")
        .collect();
    assert_eq!(tcp.len(), 1);
    assert_eq!(tcp[0]["bytes_sent"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_lines_are_sampled() {
    let backend = util::backend("hello").await;
    let (admin, marker, half) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let log_path = directory.path().join("access.log");
    start(
        directory.path(),
        admin,
        &format!(
            r#"
[admin]
listen = "{admin}"

[log]
format = "json"
output = '{log}'
status = [404]
sample_ratio = 0.5

[[server]]
name = "half"
listen = "{half}"
forward = "{backend}"

[[server]]
name = "marker"
listen = "{marker}"
[[server.match]]
serve = "/nonexistent-rxh-dir"
"#,
            log = log_path.display()
        ),
    )
    .await;

    for _ in 0..200 {
        let (status, ..) = util::get(half, "/").await;
        assert_eq!(status, StatusCode::OK);
    }
    // Matches the filter: written whatever the sampling decides.
    let (status, ..) = util::get(marker, "/marker").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    util::eventually(async || read(&log_path).iter().any(|line| line["uri"] == "/marker")).await;
    let lines = read(&log_path);
    let sampled = lines.iter().filter(|line| line["server"] == "half").count();
    assert!((50..=150).contains(&sampled), "{sampled} of 200 sampled");
}
