//! End to end tests of the `rxh` binary: daemon, admin API and CLI commands.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::Response;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address")
}

async fn backend(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::task::spawn(async move {
                let service = service_fn(move |_| async move {
                    Ok::<_, Infallible>(Response::new(Full::<Bytes>::from(tag)))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

async fn try_get(address: SocketAddr) -> Option<String> {
    let stream = TcpStream::connect(address).await.ok()?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::task::spawn(connection);
    let request = hyper::Request::get("/").body(Empty::<Bytes>::new()).ok()?;
    let response = sender.send_request(request).await.ok()?;
    let body = response.into_body().collect().await.ok()?.to_bytes();
    Some(String::from_utf8_lossy(&body).into_owned())
}

async fn wait_for_body(address: SocketAddr, expected: &str) {
    for _ in 0..250 {
        if try_get(address).await.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{address} never answered {expected:?}");
}

/// Runs a CLI command against the daemon.
fn rxh(directory: &Path, admin: SocketAddr, args: &[&str]) -> Output {
    Command::new(RXH)
        .current_dir(directory)
        .args(["--admin", &admin.to_string()])
        .args(args)
        .output()
        .expect("rxh runs")
}

fn assert_success(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_daemon(directory: &Path) -> Daemon {
    Daemon(
        Command::new(RXH)
            .current_dir(directory)
            .args(["run", "--config", "rxh.toml"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_manages_a_running_daemon() {
    let first = backend("first").await;
    let second = backend("second").await;
    let tcp_backend = backend("over-tcp").await;
    let admin = free_port();
    let web = free_port();
    let tcp = free_port();

    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "# Comment that must survive CLI edits.\n[admin]\nlisten = \"{admin}\"\n\n\
             [upstream.api]\npolicy = \"round_robin\"\nbackends = [\"{first}\"]\n\n\
             [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"@api\"\n"
        ),
    )
    .expect("write config");

    let mut daemon = start_daemon(dir);
    wait_for_body(web, "first").await;

    // Status through the admin API.
    let status = assert_success(&rxh(dir, admin, &["status", "--json"]));
    let status: serde_json::Value = serde_json::from_str(&status).expect("status is JSON");
    assert_eq!(status["servers"][0]["name"], "web");
    assert_eq!(status["pools"][0]["name"], "api");

    // Add a peer: live and persisted.
    let second_text = second.to_string();
    assert_success(&rxh(dir, admin, &["upstream", "add", "api", &second_text]));
    wait_for_body(web, "second").await;
    let saved = std::fs::read_to_string(dir.join("rxh.toml")).expect("read config");
    assert!(saved.contains(&second_text));
    assert!(saved.contains("Comment that must survive CLI edits."));

    // Duplicates and unknown pools are rejected.
    let duplicate = rxh(dir, admin, &["upstream", "add", "api", &second_text]);
    assert!(!duplicate.status.success());
    let unknown = rxh(dir, admin, &["upstream", "add", "nope", "127.0.0.1:1"]);
    assert!(!unknown.status.success());

    // Remove the first peer: only the second one answers.
    assert_success(&rxh(
        dir,
        admin,
        &["upstream", "rm", "api", &first.to_string()],
    ));
    for _ in 0..4 {
        assert_eq!(try_get(web).await.as_deref(), Some("second"));
    }

    // New pool and TCP service, applied with a reload.
    let tcp_backend_text = tcp_backend.to_string();
    let pool_args = [
        "pool",
        "add",
        "db",
        "--policy",
        "least_conn",
        "--backend",
        &tcp_backend_text,
    ];
    assert_success(&rxh(dir, admin, &pool_args));
    let tcp_text = tcp.to_string();
    let service_args = [
        "service",
        "add",
        "--listen",
        &tcp_text,
        "--protocol",
        "tcp",
        "--to",
        "@db",
        "--name",
        "raw",
    ];
    assert_success(&rxh(dir, admin, &service_args));
    wait_for_body(tcp, "over-tcp").await;
    let list = assert_success(&rxh(dir, admin, &["service", "list"]));
    assert!(list.contains("raw") && list.contains("tcp"), "{list}");

    // A pool in use cannot be removed and the file stays valid.
    assert!(!rxh(dir, admin, &["pool", "rm", "db"]).status.success());
    assert_success(&rxh(dir, admin, &["validate", "--config", "rxh.toml"]));

    // Stop through the admin API.
    assert_success(&rxh(dir, admin, &["stop"]));
    let exited = tokio::task::spawn_blocking(move || {
        for _ in 0..100 {
            if let Ok(Some(status)) = daemon.0.try_wait() {
                return Some(status);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    })
    .await
    .expect("join");
    assert!(
        exited.is_some_and(|status| status.success()),
        "daemon did not exit cleanly"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sighup_and_reload_swap_configuration_without_downtime() {
    let first = backend("backend-1").await;
    let second = backend("backend-2").await;
    let admin = free_port();
    let web = free_port();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = |backend: SocketAddr| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        )
    };

    std::fs::write(dir.join("rxh.toml"), config(first)).expect("write");
    let daemon = start_daemon(dir);
    wait_for_body(web, "backend-1").await;

    std::fs::write(dir.join("rxh.toml"), config(second)).expect("write");
    let pid = daemon.0.id().to_string();
    let killed = Command::new("kill")
        .args(["-HUP", &pid])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    wait_for_body(web, "backend-2").await;

    // A broken file is rejected and the running configuration keeps serving.
    std::fs::write(dir.join("rxh.toml"), "[[server]]\nlisten = 1").expect("write");
    assert!(!rxh(dir, admin, &["reload"]).status.success());
    assert_eq!(try_get(web).await.as_deref(), Some("backend-2"));

    std::fs::write(dir.join("rxh.toml"), config(first)).expect("write");
    assert_success(&rxh(dir, admin, &["reload"]));
    wait_for_body(web, "backend-1").await;
}

#[test]
fn validate_reports_errors() {
    let directory = tempfile::tempdir().expect("temp dir");
    let config = "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"@missing\"\n";
    std::fs::write(directory.path().join("bad.toml"), config).expect("write");

    let output = Command::new(RXH)
        .current_dir(directory.path())
        .args(["validate", "-c", "bad.toml"])
        .output()
        .expect("rxh runs");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown upstream group @missing"),
        "{stderr}"
    );
}
