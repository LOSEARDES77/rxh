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
use rxh_test_util::free_address;
use tokio::net::{TcpListener, TcpStream};

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

async fn backend(tag: &'static str) -> SocketAddr {
    backend_at(SocketAddr::from(([127, 0, 0, 1], 0)), tag).await
}

/// A backend on `address` answering `tag`.
async fn backend_at(address: SocketAddr, tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind(address).await.expect("bind");
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
    try_get_path(address, "/").await
}

async fn try_get_path(address: SocketAddr, path: &str) -> Option<String> {
    let stream = TcpStream::connect(address).await.ok()?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::task::spawn(connection);
    let request = hyper::Request::get(path).body(Empty::<Bytes>::new()).ok()?;
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
    let admin = free_address();
    let web = free_address();
    let tcp = free_address();

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
    let admin = free_address();
    let web = free_address();
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

#[tokio::test(flavor = "multi_thread")]
async fn placeholders_take_values_from_the_environment_and_secret_files() {
    let first = backend("backend-1").await;
    let second = backend("backend-2").await;
    let admin = free_address();
    let from_env = free_address();
    let from_file = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nlisten = \"{from_env}\"\nforward = \"{{env.RXH_TEST_BACKEND}}\"\n\
             [[server]]\nlisten = \"{from_file}\"\nforward = \"{{file.secrets/backend}}\"\n"
        ),
    )
    .expect("write");
    std::fs::create_dir(dir.join("secrets")).expect("mkdir");
    std::fs::write(dir.join("secrets/backend"), format!("{second}\n")).expect("write");

    // Without the variable the daemon refuses to start.
    let output = Command::new(RXH)
        .current_dir(dir)
        .args(["validate", "-c", "rxh.toml"])
        .env_remove("RXH_TEST_BACKEND")
        .output()
        .expect("rxh runs");
    let stderr = assert_failure(&output);
    assert!(
        stderr.contains("environment variable RXH_TEST_BACKEND is not set"),
        "{stderr}"
    );

    let _daemon = Daemon(
        Command::new(RXH)
            .current_dir(dir)
            .args(["run", "--config", "rxh.toml"])
            .env("RXH_TEST_BACKEND", first.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    );
    wait_for_body(from_env, "backend-1").await;
    wait_for_body(from_file, "backend-2").await;

    // A reload reads the secret file again.
    std::fs::write(dir.join("secrets/backend"), first.to_string()).expect("write");
    assert_success(&rxh(dir, admin, &["reload"]));
    wait_for_body(from_file, "backend-1").await;

    // A missing secret is rejected and the running configuration keeps serving.
    std::fs::remove_file(dir.join("secrets/backend")).expect("remove");
    let output = rxh(dir, admin, &["reload"]);
    let (stdout, stderr) = stdout_and_stderr(&output);
    assert!(!output.status.success());
    assert!(
        format!("{stdout}{stderr}").contains("{file.secrets/backend}"),
        "{stdout}{stderr}"
    );
    assert_eq!(try_get(from_file).await.as_deref(), Some("backend-1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reloads_when_the_file_or_an_include_changes() {
    let first = backend("backend-1").await;
    let second = backend("backend-2").await;
    let admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let main = |watch: bool| {
        format!("watch = {watch}\ninclude = [\"conf.d/*.toml\"]\n[admin]\nlisten = \"{admin}\"\n")
    };
    let site =
        |backend: SocketAddr| format!("[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n");

    std::fs::create_dir(dir.join("conf.d")).expect("mkdir");
    std::fs::write(dir.join("conf.d/web.toml"), site(first)).expect("write");
    std::fs::write(dir.join("rxh.toml"), main(true)).expect("write");
    let _daemon = start_daemon(dir);
    wait_for_body(web, "backend-1").await;

    // An edited included file is picked up without `rxh reload`.
    std::fs::write(dir.join("conf.d/web.toml"), site(second)).expect("write");
    wait_for_body(web, "backend-2").await;

    // A broken file keeps the running configuration.
    std::fs::write(dir.join("conf.d/web.toml"), "[[server]]\nlisten = 1").expect("write");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(try_get(web).await.as_deref(), Some("backend-2"));
    std::fs::write(dir.join("conf.d/web.toml"), site(first)).expect("write");
    wait_for_body(web, "backend-1").await;

    // Turned off by editing the main file, which is a change too.
    std::fs::write(dir.join("rxh.toml"), main(false)).expect("write");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    std::fs::write(dir.join("conf.d/web.toml"), site(second)).expect("write");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(try_get(web).await.as_deref(), Some("backend-1"));
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

#[test]
fn validate_warns_about_deprecated_keys_in_every_file() {
    let directory = tempfile::tempdir().expect("temp dir");
    let main = "include = [\"conf.d/*.toml\"]\n\n[[server]]\nlisten = \"127.0.0.1:1\"\n\
                connections = 5\nforward = \"@web\"\n";
    let site = "[upstream.web]\nalgorithm = \"WRR\"\nbackends = [\"127.0.0.1:2\"]\n";
    std::fs::create_dir(directory.path().join("conf.d")).expect("mkdir");
    std::fs::write(directory.path().join("rxh.toml"), main).expect("write");
    std::fs::write(directory.path().join("conf.d/web.toml"), site).expect("write");
    let validate = || {
        Command::new(RXH)
            .current_dir(directory.path())
            .args(["validate", "-c", "rxh.toml"])
            .output()
            .expect("rxh runs")
    };

    let output = validate();
    assert_success(&output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "warning: rxh.toml: server[0]: key `connections` is deprecated since 0.3.0, \
         use `max_connections`",
        "conf.d/web.toml: upstream.web: key `algorithm` is deprecated since 0.3.0, use `policy`",
        "conf.d/web.toml: upstream.web: policy `WRR` is deprecated since 0.3.0, \
         use `weighted_round_robin`",
    ] {
        assert!(stderr.contains(expected), "{expected}\n{stderr}");
    }

    let migrated = main.replace("connections", "max_connections");
    std::fs::write(directory.path().join("rxh.toml"), migrated).expect("write");
    let site = site.replace("algorithm = \"WRR\"", "policy = \"weighted_round_robin\"");
    std::fs::write(directory.path().join("conf.d/web.toml"), site).expect("write");
    let output = validate();
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stderr), "");
}

fn spawn_rxh(directory: &Path, args: &[&str]) -> Daemon {
    Daemon(
        Command::new(RXH)
            .current_dir(directory)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("rxh starts"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn file_server_serves_the_directory_without_a_config() {
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(directory.path().join("hello.txt"), "hi there").expect("write");
    let listen = free_address();

    let output = Command::new(RXH)
        .args(["file-server", "--root", "/definitely/not/here"])
        .output()
        .expect("rxh runs");
    assert!(!output.status.success());

    let _server = spawn_rxh(
        directory.path(),
        &[
            "file-server",
            "--listen",
            &listen.to_string(),
            "--no-browse",
        ],
    );
    for _ in 0..250 {
        if try_get_path(listen, "/hello.txt").await.as_deref() == Some("hi there") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("file server never served hello.txt");
}

#[tokio::test(flavor = "multi_thread")]
async fn reverse_proxy_forwards_to_the_backends() {
    let first = backend("proxied").await;
    let listen = free_address();
    let directory = tempfile::tempdir().expect("temp dir");

    let _proxy = spawn_rxh(
        directory.path(),
        &[
            "reverse-proxy",
            "--from",
            &listen.to_string(),
            "--to",
            &first.to_string(),
        ],
    );
    wait_for_body(listen, "proxied").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn includes_are_edited_in_the_file_that_defines_them() {
    let first = backend("first").await;
    let second = backend("second").await;
    let admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::create_dir(dir.join("conf.d")).expect("mkdir");
    std::fs::write(
        dir.join("rxh.toml"),
        format!("include = [\"conf.d/*.toml\"]\n[admin]\nlisten = \"{admin}\"\n"),
    )
    .expect("write");
    std::fs::write(
        dir.join("conf.d/api.toml"),
        format!(
            "[upstream.api]\npolicy = \"round_robin\"\nbackends = [\"{first}\"]\n\n\
             [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"@api\"\n"
        ),
    )
    .expect("write");

    let _daemon = start_daemon(dir);
    wait_for_body(web, "first").await;

    // The backend lands in the included file and the main file is untouched.
    let main_before = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    assert_success(&rxh(
        dir,
        admin,
        &["upstream", "add", "api", &second.to_string()],
    ));
    let included = std::fs::read_to_string(dir.join("conf.d/api.toml")).expect("read");
    assert!(included.contains(&second.to_string()), "{included}");
    assert_eq!(
        std::fs::read_to_string(dir.join("rxh.toml")).expect("read"),
        main_before
    );
    assert_success(&rxh(
        dir,
        admin,
        &["upstream", "rm", "api", &first.to_string()],
    ));
    wait_for_body(web, "second").await;

    // `--file` creates a new file and registers it in `include`.
    assert_success(&rxh(
        dir,
        admin,
        &[
            "pool",
            "add",
            "db",
            "--backend",
            "127.0.0.1:1",
            "--file",
            "extra/db.toml",
        ],
    ));
    assert!(
        std::fs::read_to_string(dir.join("extra/db.toml"))
            .expect("read")
            .contains("[upstream.db]")
    );
    let main = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    assert!(main.contains("extra/db.toml"), "{main}");

    // The merged configuration, from the daemon and from the files.
    let shown = assert_success(&rxh(dir, admin, &["config", "show", "--json"]));
    let shown: serde_json::Value = serde_json::from_str(&shown).expect("JSON");
    assert!(shown["upstream"]["db"].is_object() && shown["upstream"]["api"].is_object());
    let toml = assert_success(&rxh(dir, admin, &["config", "show"]));
    assert!(toml.contains("[upstream.api]"), "{toml}");

    // Remove the service from the file that defines it, after it was reloaded.
    assert_success(&rxh(dir, admin, &["service", "rm", "web"]));
    let included = std::fs::read_to_string(dir.join("conf.d/api.toml")).expect("read");
    assert!(!included.contains("[[server]]"), "{included}");
}

fn stdout_and_stderr(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_failure(output: &Output) -> String {
    let (stdout, stderr) = stdout_and_stderr(output);
    assert!(
        !output.status.success(),
        "command succeeded\nstdout: {stdout}\nstderr: {stderr}"
    );
    stderr
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_listings_render_tables() {
    let live = backend("live").await;
    let dead = free_address();
    let spare = free_address();
    let admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\n\
             [upstream.api]\npolicy = \"round_robin\"\nbackends = [\"{live}\", \"{dead}\"]\n\
             health = {{ interval = \"50ms\" }}\n\n\
             [upstream.spare]\nbackends = [\"{spare}\"]\n\n\
             [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"@api\"\n"
        ),
    )
    .expect("write config");
    let _daemon = start_daemon(dir);
    wait_for_body(web, "live").await;

    // The dead backend fails its TCP health check and shows as `NO`.
    let mut status = String::new();
    for _ in 0..250 {
        status = assert_success(&rxh(dir, admin, &["status"]));
        if status.contains("NO") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        status.starts_with(&format!("rxh {version}  pid ")),
        "{status}"
    );
    assert!(status.contains(&format!("admin {admin}")), "{status}");
    assert!(
        status.contains(&format!("dashboard http://{admin}/dashboard")),
        "{status}"
    );
    assert!(
        status.contains("config ") && status.contains("rxh.toml"),
        "{status}"
    );
    let server_line = status
        .lines()
        .find(|line| line.starts_with(&web.to_string()))
        .expect("server line");
    assert_eq!(
        server_line.split_whitespace().collect::<Vec<_>>(),
        [web.to_string().as_str(), "http", "web", "listening"]
    );
    assert!(status.contains("pool api (round_robin)"), "{status}");
    assert!(status.contains("ADDRESS"), "{status}");
    let health_of = |status: &str, address: SocketAddr| {
        let line = status
            .lines()
            .find(|line| line.trim_start().starts_with(&address.to_string()))
            .expect("backend line")
            .to_owned();
        line.split_whitespace()
            .nth(2)
            .expect("health column")
            .to_owned()
    };
    assert_eq!(health_of(&status, live), "yes", "{status}");
    assert_eq!(health_of(&status, dead), "NO", "{status}");

    // Emptying a pool on the daemon only: `(no backends)`.
    let removed = assert_success(&rxh(
        dir,
        admin,
        &["upstream", "rm", "spare", &spare.to_string(), "--no-save"],
    ));
    assert!(removed.contains("on the running daemon"), "{removed}");
    let saved = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    assert!(
        saved.contains(&spare.to_string()),
        "--no-save keeps the file"
    );
    let status = assert_success(&rxh(dir, admin, &["status"]));
    assert!(
        status.contains("pool spare (weighted_round_robin)\n  (no backends)"),
        "{status}"
    );

    // Listings from the daemon.
    let pools = assert_success(&rxh(dir, admin, &["pool", "list"]));
    let api = pools
        .lines()
        .find(|line| line.starts_with("api"))
        .expect("api");
    assert_eq!(
        api.split_whitespace().collect::<Vec<_>>(),
        ["api", "round_robin", "2", "backends"]
    );
    let all = assert_success(&rxh(dir, admin, &["upstream", "list"]));
    assert!(
        all.contains("pool api") && all.contains("pool spare"),
        "{all}"
    );
    let one = assert_success(&rxh(dir, admin, &["upstream", "list", "api"]));
    assert!(
        one.contains("pool api") && !one.contains("pool spare"),
        "{one}"
    );

    // Drain, maintenance and back: shown in the listings, traffic follows,
    // and the configuration file is left alone.
    let before = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    let drained = assert_success(&rxh(
        dir,
        admin,
        &["upstream", "drain", "api", &live.to_string()],
    ));
    assert_eq!(
        drained.trim(),
        format!("{live} of pool api is now drain (0 requests in flight)")
    );
    let one = assert_success(&rxh(dir, admin, &["upstream", "list", "api"]));
    assert_eq!(health_of(&one, live), "drain", "{one}");
    assert_ne!(
        try_get(web).await.as_deref(),
        Some("live"),
        "the only healthy backend is draining"
    );
    assert_success(&rxh(
        dir,
        admin,
        &["upstream", "maint", "api", &live.to_string()],
    ));
    let one = assert_success(&rxh(dir, admin, &["upstream", "list", "api"]));
    assert_eq!(health_of(&one, live), "maint", "{one}");
    assert_success(&rxh(
        dir,
        admin,
        &["upstream", "ready", "api", &live.to_string()],
    ));
    let one = assert_success(&rxh(dir, admin, &["upstream", "list", "api"]));
    assert_eq!(health_of(&one, live), "yes", "{one}");
    wait_for_body(web, "live").await;
    let after = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    assert_eq!(before, after);
    let stderr = assert_failure(&rxh(
        dir,
        admin,
        &["upstream", "drain", "api", "127.0.0.1:1"],
    ));
    assert!(stderr.contains("is not in pool api"), "{stderr}");

    // Admin API errors are reported with a failing exit code.
    let stderr = assert_failure(&rxh(dir, admin, &["upstream", "list", "nope"]));
    assert!(stderr.contains("no pool named nope"), "{stderr}");
    let stderr = assert_failure(&rxh(
        dir,
        admin,
        &["upstream", "add", "nope", "127.0.0.1:1", "--no-save"],
    ));
    assert!(stderr.contains("no pool named nope"), "{stderr}");
    let stderr = assert_failure(&rxh(
        dir,
        admin,
        &["upstream", "rm", "api", "127.0.0.1:1", "--no-save"],
    ));
    assert!(stderr.contains("is not in pool api"), "{stderr}");

    // A reload the daemon rejects says the running configuration stays.
    std::fs::write(dir.join("rxh.toml"), "[[server]]\nlisten = 1").expect("write");
    let stderr = assert_failure(&rxh(dir, admin, &["reload"]));
    assert!(
        stderr.contains("reload rejected, the running configuration is unchanged"),
        "{stderr}"
    );
}

#[test]
fn commands_fall_back_to_the_file_without_a_daemon() {
    // Nothing listens on this address.
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        "[upstream.api]\npolicy = \"round_robin\"\nbackends = [\"127.0.0.1:1\"]\n\n\
         [upstream.db]\nbackends = [\"127.0.0.1:2\"]\n\n\
         [[server]]\nname = \"web\"\nlisten = \"127.0.0.1:0\"\nforward = \"@api\"\n",
    )
    .expect("write config");
    let read_config = || std::fs::read_to_string(dir.join("rxh.toml")).expect("read");

    let stderr = assert_failure(&rxh(dir, admin, &["status"]));
    assert!(stderr.starts_with("rxh: "), "{stderr}");
    for command in ["stop", "reload"] {
        let stderr = assert_failure(&rxh(dir, admin, &[command, "--pid-file", "missing.pid"]));
        assert!(
            stderr.contains(&format!("no daemon on {admin} and no usable PID file")),
            "{command}: {stderr}"
        );
    }

    let listed = assert_success(&rxh(dir, admin, &["upstream", "list"]));
    assert!(
        listed.starts_with("(daemon not running, showing the configuration file)"),
        "{listed}"
    );
    assert!(
        listed.contains("pool api (round_robin)\n  127.0.0.1:1"),
        "{listed}"
    );
    assert!(listed.contains("pool db"), "{listed}");
    let only = assert_success(&rxh(dir, admin, &["upstream", "list", "db"]));
    assert!(
        only.contains("pool db") && !only.contains("pool api"),
        "{only}"
    );
    let pools = assert_success(&rxh(dir, admin, &["pool", "list"]));
    assert!(
        pools.contains("pool api") && pools.contains("pool db"),
        "{pools}"
    );

    // Edits are saved and applied on the next start.
    let next_start = "Daemon not running, the change applies on the next start";
    let added = assert_success(&rxh(dir, admin, &["upstream", "add", "api", "127.0.0.1:3"]));
    assert!(added.contains(next_start), "{added}");
    assert!(read_config().contains("127.0.0.1:3"));
    let removed = assert_success(&rxh(dir, admin, &["upstream", "rm", "api", "127.0.0.1:3"]));
    assert!(removed.contains(next_start), "{removed}");
    assert!(!read_config().contains("127.0.0.1:3"));
    let created = assert_success(&rxh(
        dir,
        admin,
        &["pool", "add", "cache", "--backend", "127.0.0.1:4"],
    ));
    assert!(created.contains(next_start), "{created}");
    let deleted = assert_success(&rxh(dir, admin, &["pool", "rm", "cache"]));
    assert!(deleted.contains(next_start), "{deleted}");
    assert!(!read_config().contains("cache"));

    // `--no-save` needs a daemon; nothing is written.
    let before = read_config();
    let stderr = assert_failure(&rxh(
        dir,
        admin,
        &["upstream", "add", "api", "127.0.0.1:5", "--no-save"],
    ));
    assert!(stderr.starts_with("rxh: "), "{stderr}");
    assert_failure(&rxh(
        dir,
        admin,
        &["upstream", "rm", "api", "127.0.0.1:1", "--no-save"],
    ));
    assert_eq!(read_config(), before);

    let stderr = assert_failure(&rxh(dir, admin, &["service", "rm", "nothing"]));
    assert!(
        stderr.contains("no service named or listening on nothing"),
        "{stderr}"
    );
    let services = assert_success(&rxh(dir, admin, &["service", "list"]));
    assert!(
        services.contains("web") && services.contains("@api"),
        "{services}"
    );

    let shown = assert_success(&rxh(dir, admin, &["config", "show"]));
    assert!(shown.contains("[upstream.api]"), "{shown}");
    let shown = assert_success(&rxh(dir, admin, &["config", "show", "--json"]));
    let shown: serde_json::Value = serde_json::from_str(&shown).expect("JSON");
    assert!(shown["upstream"]["db"].is_object(), "{shown}");

    let validated = assert_success(&rxh(dir, admin, &["validate"]));
    assert!(
        validated.contains("rxh.toml: OK (1 services, 1 listeners, 2 pools)"),
        "{validated}"
    );
    let policies = assert_success(&rxh(dir, admin, &["policies"]));
    for policy in ["round_robin", "least_conn", "cookie", "ip_hash"] {
        assert!(policies.contains(policy), "{policies}");
    }
}

/// Process of a daemon started with `rxh start`, killed on drop if a test
/// fails before stopping it.
#[cfg(unix)]
struct Background(Option<u32>);

#[cfg(unix)]
impl Drop for Background {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill runs")
        .success()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn start_reload_and_stop_through_the_pid_file() {
    let first = backend("backend-1").await;
    let second = backend("backend-2").await;
    // The admin API is disabled, so the CLI falls back to the PID file.
    let unused_admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = |backend: SocketAddr| {
        format!(
            "[admin]\nenabled = false\n[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        )
    };
    std::fs::write(dir.join("rxh.toml"), config(first)).expect("write");
    let pid_args = ["--pid-file", "run/nested/rxh.pid"];

    // A broken configuration fails in the foreground, before forking.
    std::fs::write(dir.join("broken.toml"), "[[server]]\nlisten = 1").expect("write");
    let broken = ["start", "--config", "broken.toml", pid_args[0], pid_args[1]];
    assert_failure(&rxh(dir, unused_admin, &broken));
    assert!(
        !dir.join("run").exists(),
        "nothing created for a broken file"
    );

    let start = [
        "start",
        "--config",
        "rxh.toml",
        pid_args[0],
        pid_args[1],
        "--log-file",
        "logs/rxh.log",
    ];
    let started = assert_success(&rxh(dir, unused_admin, &start));
    assert!(
        started.starts_with("Starting rxh in the background"),
        "{started}"
    );
    // The PID is written before `rxh start` returns.
    let pid: u32 = std::fs::read_to_string(dir.join("run/nested/rxh.pid"))
        .expect("PID file written in a created directory")
        .trim()
        .parse()
        .expect("numeric PID");
    let _guard = Background(Some(pid));
    assert!(is_alive(pid));
    wait_for_body(web, "backend-1").await;

    // The daemon holds a lock on its PID file: a second one fails in the
    // foreground and leaves the file alone.
    let stderr = assert_failure(&rxh(dir, unused_admin, &start));
    assert!(
        stderr.contains("is locked, is rxh already running?"),
        "{stderr}"
    );
    let unchanged = std::fs::read_to_string(dir.join("run/nested/rxh.pid")).expect("PID file");
    assert_eq!(unchanged, format!("{pid}\n"));

    std::fs::write(dir.join("rxh.toml"), config(second)).expect("write");
    let mut reload_args = vec!["reload"];
    reload_args.extend(pid_args);
    let reloaded = assert_success(&rxh(dir, unused_admin, &reload_args));
    assert_eq!(reloaded.trim(), format!("Sent SIGHUP to {pid}"));
    wait_for_body(web, "backend-2").await;

    let mut stop_args = vec!["stop"];
    stop_args.extend(pid_args);
    let stopped = assert_success(&rxh(dir, unused_admin, &stop_args));
    assert_eq!(stopped.trim(), format!("Sent SIGTERM to {pid}"));
    for _ in 0..250 {
        if !is_alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!is_alive(pid), "daemon exited on SIGTERM");
    assert!(try_get(web).await.is_none(), "listener closed");

    // The daemon's output went to the log file.
    let log = std::fs::read_to_string(dir.join("logs/rxh.log")).expect("log file");
    assert!(log.contains("Main => Reloading"), "{log}");
    assert!(log.contains("Main => Shutting down"), "{log}");
}

#[test]
fn start_refuses_a_busy_admin_address() {
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let admin = squatter.local_addr().expect("address");
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!("[admin]\nlisten = \"{admin}\"\n"),
    )
    .expect("write");

    let stderr = assert_failure(&rxh(
        dir,
        admin,
        &["start", "--config", "rxh.toml", "--pid-file", "rxh.pid"],
    ));
    assert!(
        stderr.contains(&format!("admin address {admin} is busy")),
        "{stderr}"
    );
    assert!(!dir.join("rxh.pid").exists());
}

/// Number of `rxh-core-N` threads of the process, the per core runtimes.
#[cfg(target_os = "linux")]
fn per_core_threads(pid: u32) -> usize {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return 0;
    };
    tasks
        .filter_map(Result::ok)
        .filter_map(|task| std::fs::read_to_string(task.path().join("comm")).ok())
        .filter(|name| name.starts_with("rxh-core-"))
        .count()
}

/// Waits until the process has `expected` per core runtimes.
#[cfg(target_os = "linux")]
async fn wait_for_per_core_threads(pid: u32, expected: usize) {
    for _ in 0..250 {
        if per_core_threads(pid) == expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "{} per core runtimes, expected {expected}",
        per_core_threads(pid)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn per_core_runtime_serves_reloads_and_stops() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Counts the connections rxh opens to it.
    let connections = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let counted = listener.local_addr().expect("address");
    let counter = Arc::clone(&connections);
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::task::spawn(async move {
                let service = service_fn(|_| async {
                    Ok::<_, Infallible>(Response::new(Full::<Bytes>::from("counted")))
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    let second = backend("second").await;

    let admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = |runtime: &str, backend: SocketAddr| {
        format!(
            "runtime = \"{runtime}\"\n[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        )
    };
    std::fs::write(dir.join("rxh.toml"), config("per_core", counted)).expect("write");
    let mut daemon = start_daemon(dir);
    let pid = daemon.0.id();
    wait_for_body(web, "counted").await;

    let cpus = std::thread::available_parallelism().expect("known").get();
    // One runtime per CPU; the first one answers while the rest start.
    wait_for_per_core_threads(pid, cpus).await;

    // Every runtime answers, and keeps its own upstream connections: the
    // kernel spreads these client connections over the listeners.
    for _ in 0..200 {
        assert_eq!(try_get(web).await.as_deref(), Some("counted"));
    }
    let opened = connections.load(Ordering::SeqCst);
    assert!(opened <= cpus, "{opened} upstream connections, {cpus} CPUs");
    if cpus > 1 {
        assert!(opened > 1, "the runtimes share one upstream connection");
    }

    // The listener is reported once, not once per runtime.
    let status = assert_success(&rxh(dir, admin, &["status", "--json"]));
    let status: serde_json::Value = serde_json::from_str(&status).expect("status is JSON");
    assert_eq!(status["servers"].as_array().expect("servers").len(), 1);

    // Reloads swap the runtimes: the old threads end once drained.
    std::fs::write(dir.join("rxh.toml"), config("per_core", second)).expect("write");
    assert_success(&rxh(dir, admin, &["reload"]));
    wait_for_body(web, "second").await;
    wait_for_per_core_threads(pid, cpus).await;

    // A reload can go back to the shared runtime.
    std::fs::write(dir.join("rxh.toml"), config("shared", counted)).expect("write");
    assert_success(&rxh(dir, admin, &["reload"]));
    wait_for_body(web, "counted").await;
    wait_for_per_core_threads(pid, 0).await;

    std::fs::write(dir.join("rxh.toml"), config("per_core", second)).expect("write");
    assert_success(&rxh(dir, admin, &["reload"]));
    wait_for_body(web, "second").await;

    assert_success(&rxh(dir, admin, &["stop"]));
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || daemon.0.wait()),
    )
    .await
    .expect("the daemon exits")
    .expect("no panic")
    .expect("waited");
    assert!(status.success(), "{status}");
}

#[test]
fn per_core_runtime_fails_to_start_on_a_busy_address() {
    // Held without `SO_REUSEPORT`, so rxh cannot share it.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = busy.local_addr().expect("address");
    let directory = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        directory.path().join("rxh.toml"),
        format!(
            "runtime = \"per_core\"\n[admin]\nenabled = false\n\
             [[server]]\nlisten = \"{address}\"\nforward = \"127.0.0.1:1\"\n"
        ),
    )
    .expect("write");

    let output = Command::new(RXH)
        .current_dir(directory.path())
        .args(["run", "--config", "rxh.toml"])
        .output()
        .expect("rxh runs");
    let stderr = assert_failure(&output);
    assert!(stderr.contains("in use"), "{stderr}");

    let output = Command::new(RXH)
        .current_dir(directory.path())
        .args(["validate", "--config", "rxh.toml"])
        .output()
        .expect("rxh runs");
    assert_success(&output);
    std::fs::write(
        directory.path().join("rxh.toml"),
        "runtime = \"per-core\"\n",
    )
    .expect("write");
    let output = Command::new(RXH)
        .current_dir(directory.path())
        .args(["validate", "--config", "rxh.toml"])
        .output()
        .expect("rxh runs");
    assert!(assert_failure(&output).contains("runtime"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cert_commands_list_and_reload() {
    let admin = free_address();
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let certificate = rxh_test_util::OcspCertificate::new(None);
    std::fs::write(dir.join("chain.pem"), &certificate.chain_pem).expect("write chain");
    std::fs::write(dir.join("key.pem"), &certificate.key_pem).expect("write key");
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\n\
             [[server]]\nlisten = \"{web}\"\nprotocol = \"tcp\"\nhost = \"tls.test\"\n\
             forward = \"{}\"\ntls = {{ cert = \"chain.pem\", key = \"key.pem\" }}\n",
            free_address()
        ),
    )
    .expect("write config");
    let _daemon = start_daemon(dir);
    for _ in 0..250 {
        if TcpStream::connect(web).await.is_ok() && TcpStream::connect(admin).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let list = assert_success(&rxh(dir, admin, &["cert", "list"]));
    let mut lines = list.lines();
    let header = lines.next().expect("header");
    assert!(
        header.starts_with("LISTEN") && header.contains("EXPIRES"),
        "{list}"
    );
    let row = lines.next().expect("one certificate");
    assert!(row.starts_with(&web.to_string()), "{row}");
    assert!(row.contains("tls.test (default)"), "{row}");
    assert!(row.ends_with("chain.pem"), "{row}");
    assert!(!row.contains("EXPIRED"), "{row}");

    let json = assert_success(&rxh(dir, admin, &["cert", "ls", "--json"]));
    let json: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    assert_eq!(json[0]["names"], serde_json::json!(["tls.test"]));

    let reload = assert_success(&rxh(dir, admin, &["cert", "reload"]));
    assert_eq!(reload.trim(), "Reloaded chain.pem");

    std::fs::write(dir.join("key.pem"), "broken").expect("break the key");
    let stderr = assert_failure(&rxh(dir, admin, &["cert", "reload"]));
    assert!(stderr.contains("certificate reload failed"), "{stderr}");
    assert!(stderr.contains("key.pem"), "{stderr}");
}

#[test]
fn config_show_without_a_daemon_masks_the_admin_secrets() {
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let admin = free_address();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\ntoken = \"file-token-0123456789\"\n\
             public_url = \"http://{admin}\"\n\
             [admin.oidc]\nissuer = \"https://id.example.com/\"\nclient_id = \"rxh\"\n\
             client_secret = \"oidc-client-secret\"\n"
        ),
    )
    .expect("write");
    // Nothing listens on `admin`: the command reads the file.
    for args in [&["config", "show"][..], &["config", "show", "--json"]] {
        let shown = assert_success(&rxh_bare(dir, args));
        assert!(shown.contains("id.example.com/"), "{shown}");
        assert!(shown.contains("***"), "{shown}");
        assert!(
            !shown.contains("file-token-0123456789") && !shown.contains("oidc-client-secret"),
            "{shown}"
        );
    }
}

/// Runs a CLI command with nothing but the configuration file of `directory`.
fn rxh_bare(directory: &Path, args: &[&str]) -> Output {
    Command::new(RXH)
        .current_dir(directory)
        .env_remove("RXH_ADMIN")
        .env_remove("RXH_ADMIN_TOKEN")
        .env_remove("RXH_CONFIG")
        .args(args)
        .output()
        .expect("rxh runs")
}

fn stderr_of(output: &Output) -> String {
    assert!(!output.status.success(), "command must fail");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn cli_talks_to_the_daemon_over_a_unix_socket() {
    let first = backend("first").await;
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let socket = dir.join("admin.sock");
    let address = format!("unix/{}", socket.display());
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{address}\"\n\n[upstream.api]\nbackends = [\"{first}\"]\n\n\
             [[server]]\nlisten = \"{}\"\nforward = \"@api\"\n",
            free_address()
        ),
    )
    .expect("write");
    let _daemon = start_daemon(dir);
    for _ in 0..250 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The address comes from the configuration file, or from `--admin`.
    let status = assert_success(&rxh_bare(dir, &["status"]));
    assert!(status.contains(&format!("admin {address}")), "{status}");
    let elsewhere = tempfile::tempdir().expect("temp dir");
    let status = assert_success(&rxh_bare(
        elsewhere.path(),
        &["--admin", &address, "status"],
    ));
    assert!(status.contains(&format!("admin {address}")), "{status}");

    // Live changes and reloads go through the socket too.
    assert_success(&rxh_bare(dir, &["upstream", "add", "api", "127.0.0.1:1"]));
    let list = assert_success(&rxh_bare(dir, &["upstream", "list", "api"]));
    assert!(list.contains("127.0.0.1:1"), "{list}");

    // A second daemon is refused in the foreground.
    let pid_file = dir.join("second.pid");
    let pid_file = pid_file.to_str().expect("utf-8 path");
    let busy = stderr_of(&rxh_bare(dir, &["--pid-file", pid_file, "start"]));
    assert!(
        busy.contains(&format!("admin address {address} is busy")),
        "{busy}"
    );

    let missing = stderr_of(&rxh_bare(
        dir,
        &["--admin", "unix//nonexistent/admin.sock", "status"],
    ));
    assert!(missing.contains("daemon is not reachable"), "{missing}");
    let relative = stderr_of(&rxh_bare(dir, &["--admin", "unix/admin.sock", "status"]));
    assert!(relative.contains("invalid admin address"), "{relative}");

    assert_success(&rxh_bare(dir, &["stop"]));
    assert!(!socket.exists(), "the socket file is removed on stop");
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_sends_the_admin_token() {
    let token = "cli-admin-token-0123456789";
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{token}\"\n"),
    )
    .expect("write");
    let _daemon = start_daemon(dir);
    for _ in 0..250 {
        if TcpStream::connect(admin).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let admin = admin.to_string();

    // The token of the configuration file is used for the address of that file.
    assert_success(&rxh_bare(dir, &["status"]));
    assert_success(&rxh_bare(dir, &["--admin", &admin, "status"]));
    let shown = assert_success(&rxh_bare(dir, &["config", "show", "--json"]));
    assert!(
        !shown.contains(token),
        "the daemon redacts the token: {shown}"
    );

    // Without the file, the token comes from the flag or the environment.
    let elsewhere = tempfile::tempdir().expect("temp dir");
    let remote = elsewhere.path();
    let denied = stderr_of(&rxh_bare(remote, &["--admin", &admin, "status"]));
    assert!(denied.contains("401"), "{denied}");
    assert!(denied.contains("Authorization: Bearer"), "{denied}");
    let wrong = [
        "--admin",
        &admin,
        "--admin-token",
        "not-the-right-token",
        "stop",
    ];
    let denied = stderr_of(&rxh_bare(remote, &wrong));
    assert!(denied.contains("401"), "{denied}");
    assert_success(&rxh_bare(
        remote,
        &["--admin", &admin, "--admin-token", token, "status"],
    ));
    let from_env = Command::new(RXH)
        .current_dir(remote)
        .env_remove("RXH_CONFIG")
        .env("RXH_ADMIN", &admin)
        .env("RXH_ADMIN_TOKEN", token)
        .arg("stop")
        .output()
        .expect("rxh runs");
    assert_success(&from_env);
}

#[test]
fn remote_admin_without_a_token_does_not_start() {
    let directory = tempfile::tempdir().expect("temp dir");
    let config = "[admin]\nlisten = \"0.0.0.0:2019\"\n";
    std::fs::write(directory.path().join("rxh.toml"), config).expect("write");

    let stderr = stderr_of(&rxh_bare(directory.path(), &["run"]));
    assert!(stderr.contains("set token"), "{stderr}");
}

/// Starts `rxh run` the way systemd does with socket activation: `sockets`
/// become descriptors 3, 4... and `LISTEN_PID` names the daemon itself.
#[cfg(target_os = "linux")]
fn start_socket_activated(directory: &Path, sockets: [std::os::fd::OwnedFd; 2]) -> Daemon {
    let [first, second] = sockets;
    // Standard input and output carry the sockets past the close-on-exec of
    // every other descriptor; the shell moves them to 3 and 4 and execs rxh,
    // which keeps its process id.
    let script = "exec 3<&0 4>&1 0</dev/null 1>/dev/null; \
                  LISTEN_PID=$$ LISTEN_FDS=2 exec \"$0\" run --config rxh.toml";
    Daemon(
        Command::new("sh")
            .current_dir(directory)
            .args(["-c", script, RXH])
            .stdin(Stdio::from(first))
            .stdout(Stdio::from(second))
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    )
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn socket_activation_serves_on_the_sockets_systemd_passes() {
    let first = backend("first").await;
    let second = backend("second").await;
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();

    // Held by the test, without SO_REUSEPORT: rxh cannot bind them itself.
    let web = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let web_address = web.local_addr().expect("address");
    let socket = dir.join("admin.sock");
    let admin = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
    let address = format!("unix/{}", socket.display());
    let config = |backend: SocketAddr| {
        format!(
            "[admin]\nlisten = \"{address}\"\n\
             [[server]]\nlisten = \"{web_address}\"\nforward = \"{backend}\"\n"
        )
    };
    std::fs::write(dir.join("rxh.toml"), config(first)).expect("write");

    let mut daemon = start_socket_activated(dir, [web.into(), admin.into()]);
    wait_for_body(web_address, "first").await;
    let status = assert_success(&rxh_bare(dir, &["status"]));
    assert!(status.contains(&web_address.to_string()), "{status}");

    // A reload takes the same socket again.
    std::fs::write(dir.join("rxh.toml"), config(second)).expect("write");
    assert_success(&rxh_bare(dir, &["reload"]));
    wait_for_body(web_address, "second").await;

    // The socket file belongs to systemd: rxh leaves it on exit.
    assert_success(&rxh_bare(dir, &["stop"]));
    let exited = daemon.0.wait().expect("daemon exits");
    assert!(exited.success(), "{exited}");
    assert!(socket.exists());
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn sd_notify_reports_readiness_reloads_and_stopping() {
    use std::os::unix::net::UnixDatagram;

    let backend = backend("backend").await;
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = format!(
        "[admin]\nenabled = false\n[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
    );
    std::fs::write(dir.join("rxh.toml"), &config).expect("write");
    let notify = dir.join("notify.sock");
    let receiver = UnixDatagram::bind(&notify).expect("bind");
    receiver
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let receive = || {
        let mut buffer = [0; 512];
        let length = receiver.recv(&mut buffer).expect("notification");
        String::from_utf8_lossy(&buffer[..length]).into_owned()
    };

    let daemon = Daemon(
        Command::new(RXH)
            .current_dir(dir)
            .args(["run", "--config", "rxh.toml"])
            .env("NOTIFY_SOCKET", &notify)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    );
    assert_eq!(receive(), "READY=1\nSTATUS=Running");
    // Ready means serving.
    assert_eq!(try_get(web).await.as_deref(), Some("backend"));

    let pid = daemon.0.id().to_string();
    let signal = |name: &str| {
        let killed = Command::new("kill")
            .args([name, &pid])
            .status()
            .expect("kill runs");
        assert!(killed.success());
    };
    signal("-HUP");
    let reloading = receive();
    assert!(
        reloading.starts_with("RELOADING=1\nMONOTONIC_USEC="),
        "{reloading}"
    );
    assert_eq!(receive(), "READY=1\nSTATUS=Running");

    // A failed reload keeps the old configuration, so the service stays ready.
    std::fs::write(dir.join("rxh.toml"), "[[server]]\nlisten = 1").expect("write");
    signal("-HUP");
    assert!(receive().starts_with("RELOADING=1\n"));
    let failed = receive();
    assert!(
        failed.starts_with("READY=1\nSTATUS=Reload failed: "),
        "{failed}"
    );
    assert!(!failed["READY=1\n".len()..].contains('\n'), "{failed}");

    signal("-TERM");
    assert_eq!(receive(), "STOPPING=1");
}

/// One entry of the Docker container list: a container named `id` whose only
/// port is the one of `backend`, with `rxh.enable=true` and `labels`.
fn docker_container(id: &str, backend: SocketAddr, labels: &[(&str, &str)]) -> serde_json::Value {
    let mut all = serde_json::Map::new();
    all.insert("rxh.enable".into(), "true".into());
    for (key, value) in labels {
        all.insert((*key).into(), (*value).into());
    }
    serde_json::json!({
        "Id": id,
        "Names": [format!("/{id}")],
        "Labels": all,
        "Ports": [{ "PrivatePort": backend.port(), "Type": "tcp" }],
        "HostConfig": { "NetworkMode": "bridge" },
        "NetworkSettings": { "Networks": { "bridge": { "IPAddress": backend.ip().to_string() } } },
    })
}

fn docker_list(containers: &[&serde_json::Value]) -> String {
    serde_json::to_string(containers).expect("serializes")
}

async fn get_host(address: SocketAddr, host: &str) -> Option<String> {
    let stream = TcpStream::connect(address).await.ok()?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::task::spawn(connection);
    let request = hyper::Request::get("/")
        .header("host", host)
        .body(Empty::<Bytes>::new())
        .ok()?;
    let response = sender.send_request(request).await.ok()?;
    let body = response.into_body().collect().await.ok()?.to_bytes();
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Waits until `tries` requests in a row for `host` are answered by exactly
/// the backends in `expected`, each of them at least once.
async fn wait_for_backends(address: SocketAddr, host: &str, expected: &[&str], tries: usize) {
    let mut last = Vec::new();
    for _ in 0..100 {
        let mut seen = Vec::new();
        for _ in 0..tries {
            let body = get_host(address, host).await.unwrap_or_default();
            if !seen.contains(&body) {
                seen.push(body);
            }
        }
        seen.sort();
        if seen == expected {
            return;
        }
        last = seen;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{host} on {address} is served by {last:?}, expected {expected:?}");
}

/// Starts the daemon with its standard output in `stdout.log`.
fn start_daemon_logged(directory: &Path) -> Daemon {
    let log = std::fs::File::create(directory.join("stdout.log")).expect("log file");
    Daemon(
        Command::new(RXH)
            .current_dir(directory)
            .args(["run", "--config", "rxh.toml"])
            .stdout(log)
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn docker_containers_are_routed_by_their_labels_and_followed_live() {
    let docker = rxh_test_util::DockerApi::start().await;
    let (a, b, db, file) = (
        backend("a").await,
        backend("b").await,
        backend("db").await,
        backend("file").await,
    );
    let (admin, web, tcp) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [docker]\nendpoint = \"{}\"\nlisten = \"{web}\"\n\
             [[server]]\nlisten = \"{web}\"\nhost = \"file.test\"\nforward = \"{file}\"\n",
            docker.endpoint()
        ),
    )
    .expect("write config");
    // How often the listeners were rebuilt for Docker.
    let route_updates = || {
        std::fs::read_to_string(dir.join("stdout.log"))
            .expect("log")
            .matches("Docker containers changed, updating the routes")
            .count()
    };

    let app = [("rxh.http.host", "app.test"), ("rxh.pool", "app")];
    let app_a = docker_container("app-a", a, &app);
    let app_b = docker_container("app-b", b, &app);
    // Running before the daemon starts.
    docker.publish(&docker_list(&[&app_a]), "start", "app-a");
    let _daemon = start_daemon_logged(dir);
    wait_for_backends(web, "app.test", &["a"], 1).await;
    // Servers of the file share the address with the ones of the containers.
    assert_eq!(get_host(web, "file.test").await.as_deref(), Some("file"));
    assert_eq!(route_updates(), 1);

    let pools = assert_success(&rxh(dir, admin, &["status"]));
    assert!(
        pools.contains("app") && pools.contains(&a.to_string()),
        "{pools}"
    );

    // A replica joins the pool without rebuilding the listeners.
    docker.publish(&docker_list(&[&app_a, &app_b]), "start", "app-b");
    wait_for_backends(web, "app.test", &["a", "b"], 20).await;
    assert_eq!(route_updates(), 1);

    // A container that died leaves at once, even while Docker still lists it.
    docker.publish(&docker_list(&[&app_a, &app_b]), "die", "app-a");
    wait_for_backends(web, "app.test", &["b"], 20).await;
    assert_eq!(route_updates(), 1);

    // A TCP route, next to a container whose labels cannot be routed.
    let database = docker_container("db", db, &[("rxh.tcp.listen", &tcp.to_string())]);
    let broken = docker_container(
        "broken",
        db,
        &[("rxh.http.host", "broken.test"), ("rxh.tcp.listen", ":1")],
    );
    docker.publish(&docker_list(&[&app_b, &database, &broken]), "start", "db");
    wait_for_body(tcp, "db").await;
    assert_eq!(route_updates(), 2);
    wait_for_backends(web, "app.test", &["b"], 5).await;
    assert_ne!(get_host(web, "broken.test").await.as_deref(), Some("db"));

    // The last containers stop: their routes go away, the file's stay.
    docker.publish("[]", "die", "app-b");
    for _ in 0..250 {
        if get_host(web, "app.test").await.as_deref() != Some("b") && try_get(tcp).await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(get_host(web, "app.test").await.as_deref(), Some("b"));
    assert_eq!(try_get(tcp).await, None);
    assert_eq!(get_host(web, "file.test").await.as_deref(), Some("file"));
}

#[tokio::test(flavor = "multi_thread")]
async fn podman_containers_are_routed_through_their_published_ports() {
    let podman = rxh_test_util::DockerApi::start().await;
    let published = backend("published").await;
    let (admin, web) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = |provider: &str| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [docker]\nprovider = \"{provider}\"\nendpoint = \"{}\"\nlisten = \"{web}\"\n",
            podman.endpoint()
        )
    };
    std::fs::write(dir.join("rxh.toml"), config("podman")).expect("write config");

    // A rootless container: no address on a network, port 80 published on
    // the port of the backend.
    let container = serde_json::json!({
        "Id": "rootless",
        "Names": ["rootless"],
        "Labels": { "rxh.enable": "true", "rxh.http.host": "app.test" },
        "Ports": [{ "IP": "0.0.0.0", "PrivatePort": 80, "PublicPort": published.port(), "Type": "tcp" }],
        "HostConfig": { "NetworkMode": "pasta" },
        "NetworkSettings": { "Networks": {} },
    });
    podman.publish(&docker_list(&[&container]), "start", "rootless");
    let _daemon = start_daemon(dir);
    wait_for_backends(web, "app.test", &["published"], 1).await;

    podman.publish("[]", "die", "rootless");
    for _ in 0..250 {
        if get_host(web, "app.test").await.as_deref() != Some("published") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(
        get_host(web, "app.test").await.as_deref(),
        Some("published")
    );

    // Docker reaches containers on their network only: this one is ignored.
    podman.publish(&docker_list(&[&container]), "start", "rootless");
    std::fs::write(dir.join("rxh.toml"), config("docker")).expect("write config");
    assert_success(&rxh(dir, admin, &["reload"]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_ne!(
        get_host(web, "app.test").await.as_deref(),
        Some("published")
    );
}

/// One Swarm task of the service `s1`, running or not, at `ip`.
fn swarm_task(id: &str, slot: u64, state: &str, ip: &str) -> serde_json::Value {
    serde_json::json!({
        "ID": id,
        "ServiceID": "s1",
        "Slot": slot,
        "NodeID": "node",
        "Status": { "State": state },
        "NetworksAttachments": [
            { "Network": { "Spec": { "Name": "ingress", "Ingress": true } }, "Addresses": ["10.0.0.2/24"] },
            { "Network": { "Spec": { "Name": "shop_default" } }, "Addresses": [format!("{ip}/24")] },
        ],
    })
}

// Replicas of a service share a port, so the second one needs its own
// loopback address, which only Linux routes without configuration.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn swarm_services_are_routed_to_their_running_tasks() {
    let swarm = rxh_test_util::DockerApi::start().await;
    let a = backend("a").await;
    let b = backend_at(SocketAddr::from(([127, 0, 0, 2], a.port())), "b").await;
    let (admin, web) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [docker]\nprovider = \"swarm\"\nendpoint = \"{}\"\nlisten = \"{web}\"\n\
             refresh = \"50ms\"\n",
            swarm.endpoint()
        ),
    )
    .expect("write config");
    let route_updates = || {
        std::fs::read_to_string(dir.join("stdout.log"))
            .expect("log")
            .matches("Docker containers changed, updating the routes")
            .count()
    };
    let services = serde_json::json!([{
        "ID": "s1",
        "Spec": { "Name": "shop_web", "Labels": { "rxh.enable": "true", "rxh.http.host": "shop.test" } },
        "Endpoint": { "Ports": [{ "Protocol": "tcp", "TargetPort": a.port(), "PublishedPort": 8080 }] },
    }])
    .to_string();
    let tasks = |tasks: &[serde_json::Value]| serde_json::to_string(tasks).expect("serializes");

    swarm.publish_swarm(
        &services,
        &tasks(&[swarm_task("t1", 1, "running", "127.0.0.1")]),
    );
    let _daemon = start_daemon_logged(dir);
    wait_for_backends(web, "shop.test", &["a"], 1).await;

    // A second replica is picked up by the next poll, without events.
    swarm.publish_swarm(
        &services,
        &tasks(&[
            swarm_task("t1", 1, "running", "127.0.0.1"),
            swarm_task("t2", 2, "running", &b.ip().to_string()),
            swarm_task("t3", 3, "preparing", "127.0.0.3"),
        ]),
    );
    wait_for_backends(web, "shop.test", &["a", "b"], 20).await;

    // A task that stops leaves the pool.
    swarm.publish_swarm(
        &services,
        &tasks(&[
            swarm_task("t1", 1, "shutdown", "127.0.0.1"),
            swarm_task("t2", 2, "running", &b.ip().to_string()),
        ]),
    );
    wait_for_backends(web, "shop.test", &["b"], 20).await;
    assert_eq!(route_updates(), 1);

    // The service goes away with its route.
    swarm.publish_swarm("[]", "[]");
    for _ in 0..250 {
        if get_host(web, "shop.test").await.as_deref() != Some("b") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(get_host(web, "shop.test").await.as_deref(), Some("b"));
    assert_eq!(route_updates(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn docker_provider_waits_for_docker_and_stops_with_its_section() {
    let (a, file) = (backend("a").await, backend("file").await);
    let (admin, web, docker_address) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = |docker: &str| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n{docker}\
             [[server]]\nlisten = \"{web}\"\nhost = \"file.test\"\nforward = \"{file}\"\n"
        )
    };
    let with_docker =
        format!("[docker]\nendpoint = \"tcp://{docker_address}\"\nlisten = \"{web}\"\n");
    std::fs::write(dir.join("rxh.toml"), config(&with_docker)).expect("write config");

    // Nothing answers on the endpoint: the daemon serves the file anyway.
    let _daemon = start_daemon(dir);
    wait_for_backends(web, "file.test", &["file"], 1).await;

    // Docker comes up later and its containers are picked up.
    let docker = rxh_test_util::DockerApi::start_on(docker_address).await;
    let container = docker_container("app", a, &[("rxh.http.host", "app.test")]);
    docker.publish(&docker_list(&[&container]), "start", "app");
    wait_for_backends(web, "app.test", &["a"], 1).await;

    // Without `[docker]` the routes of the containers are dropped.
    std::fs::write(dir.join("rxh.toml"), config("")).expect("write config");
    assert_success(&rxh(dir, admin, &["reload"]));
    assert_ne!(get_host(web, "app.test").await.as_deref(), Some("a"));
    assert_eq!(get_host(web, "file.test").await.as_deref(), Some("file"));

    // And an invalid endpoint is refused before the daemon starts.
    std::fs::write(
        dir.join("rxh.toml"),
        config("[docker]\nendpoint = \"docker.sock\"\n"),
    )
    .expect("write config");
    let output = rxh(dir, admin, &["validate", "-c", "rxh.toml"]);
    assert!(!output.status.success());
    assert!(
        stderr_of(&output).contains("docker: endpoint must be"),
        "{}",
        stderr_of(&output)
    );
}

/// Writes `rxh.toml` with an admin API and `section`, and starts the daemon.
fn start_with_catalog(dir: &Path, section: &str) -> Daemon {
    let admin = free_address();
    std::fs::write(
        dir.join("rxh.toml"),
        format!("[admin]\nlisten = \"{admin}\"\n{section}"),
    )
    .expect("write config");
    start_daemon_logged(dir)
}

/// Lines of `stdout.log` in `dir` that contain `text`.
fn log_count(dir: &Path, text: &str) -> usize {
    std::fs::read_to_string(dir.join("stdout.log"))
        .expect("log")
        .matches(text)
        .count()
}

fn consul_entry(id: &str, backend: SocketAddr, tags: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "Node": { "Node": "node-1", "Address": backend.ip().to_string() },
        "Service": {
            "ID": id, "Service": "web", "Tags": tags,
            "Address": "", "Port": backend.port(),
        },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn consul_services_are_routed_and_follow_their_health() {
    let consul = rxh_test_util::CatalogApi::start().await;
    let (a, b) = (backend("a").await, backend("b").await);
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let tags = ["rxh.enable=true", "rxh.http.host=web.test", "v2"];
    let services = r#"{"consul":[],"web":["rxh.enable=true","rxh.http.host=web.test","v2"]}"#;
    consul.publish("GET /v1/catalog/services", services);
    let health = "GET /v1/health/service/web?passing=1";
    let entries = |entries: &[serde_json::Value]| serde_json::to_string(entries).expect("json");
    consul.publish(health, &entries(&[consul_entry("web-a", a, &tags)]));

    let _daemon = start_with_catalog(
        dir,
        &format!(
            "[consul]\nendpoint = \"{}\"\ntoken = \"s3cret\"\nlisten = \"{web}\"\n\
             refresh = \"50ms\"\n",
            consul.endpoint()
        ),
    );
    wait_for_backends(web, "web.test", &["a"], 1).await;
    assert!(
        consul
            .requests()
            .iter()
            .all(|head| head.contains("x-consul-token: s3cret")),
        "{:?}",
        consul.requests()
    );

    // A second healthy instance joins without rebuilding the listeners.
    consul.publish(
        health,
        &entries(&[
            consul_entry("web-a", a, &tags),
            consul_entry("web-b", b, &tags),
        ]),
    );
    wait_for_backends(web, "web.test", &["a", "b"], 20).await;
    // An instance whose checks fail is no longer listed as passing.
    consul.publish(health, &entries(&[consul_entry("web-b", b, &tags)]));
    wait_for_backends(web, "web.test", &["b"], 20).await;
    // Once for the first listing only.
    assert_eq!(log_count(dir, "Consul instances changed"), 1);

    // An answer that cannot be read keeps the instances last listed.
    consul.publish(health, "<html>");
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_for_backends(web, "web.test", &["b"], 20).await;

    // Untagged, the service goes away with its route.
    consul.publish("GET /v1/catalog/services", r#"{"web":["v2"]}"#);
    for _ in 0..250 {
        if get_host(web, "web.test").await.as_deref() != Some("b") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(get_host(web, "web.test").await.as_deref(), Some("b"));
    assert_eq!(log_count(dir, "Consul instances changed"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn nomad_services_are_routed_over_tcp_and_http() {
    let nomad = rxh_test_util::CatalogApi::start().await;
    let (api, db) = (backend("api").await, backend("db").await);
    let (web, tcp) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let tcp_tag = format!("rxh.tcp.listen={tcp}");
    nomad.publish(
        "GET /v1/services?namespace=*",
        &serde_json::json!([
            { "Namespace": "default", "Services": [
                { "ServiceName": "api", "Tags": ["rxh.enable=true"] },
                { "ServiceName": "batch", "Tags": [] },
            ]},
            { "Namespace": "data", "Services": [
                { "ServiceName": "db", "Tags": ["rxh.enable=true", tcp_tag] },
            ]},
        ])
        .to_string(),
    );
    let registration = |name: &str, backend: SocketAddr, tags: &[&str]| {
        serde_json::json!([{
            "ID": format!("_nomad-task-{name}"), "ServiceName": name, "AllocID": "0123456789",
            "Tags": tags, "Address": backend.ip().to_string(), "Port": backend.port(),
        }])
        .to_string()
    };
    nomad.publish(
        "GET /v1/service/api?namespace=default",
        &registration("api", api, &["rxh.enable=true", "rxh.http.host=api.test"]),
    );
    nomad.publish(
        "GET /v1/service/db?namespace=data",
        &registration(
            "db",
            db,
            &["rxh.enable=true", &format!("rxh.tcp.listen={tcp}")],
        ),
    );

    let _daemon = start_with_catalog(
        dir,
        &format!(
            "[nomad]\nendpoint = \"{}\"\ntoken = \"t0ken\"\nlisten = \"{web}\"\n\
             refresh = \"50ms\"\n",
            nomad.endpoint()
        ),
    );
    wait_for_backends(web, "api.test", &["api"], 1).await;
    wait_for_body(tcp, "db").await;
    assert!(
        nomad
            .requests()
            .iter()
            .all(|head| head.contains("x-nomad-token: t0ken"))
    );
    assert!(
        !nomad
            .requests()
            .iter()
            .any(|head| head.contains("/v1/service/batch"))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn etcd_keys_are_routed_and_followed() {
    use base64::Engine;

    let etcd = rxh_test_util::CatalogApi::start().await;
    let (a, b) = (backend("a").await, backend("b").await);
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let engine = base64::engine::general_purpose::STANDARD;
    let range = |pairs: &[(&str, String)]| {
        let kvs: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(key, value)| {
                serde_json::json!({ "key": engine.encode(key), "value": engine.encode(value) })
            })
            .collect();
        serde_json::json!({ "header": {}, "kvs": kvs }).to_string()
    };
    let host = String::from("shop.test");
    etcd.publish(
        "POST /v3/kv/range",
        &range(&[
            ("apps/shop/a/address", a.to_string()),
            ("apps/shop/a/http.host", host.clone()),
        ]),
    );

    let _daemon = start_with_catalog(
        dir,
        &format!(
            "[etcd]\nendpoint = \"{}\"\nprefix = \"apps/\"\nlisten = \"{web}\"\n\
             refresh = \"50ms\"\n",
            etcd.endpoint()
        ),
    );
    wait_for_backends(web, "shop.test", &["a"], 1).await;

    etcd.publish(
        "POST /v3/kv/range",
        &range(&[
            ("apps/shop/a/address", a.to_string()),
            ("apps/shop/a/http.host", host.clone()),
            ("apps/shop/b/address", b.to_string()),
            ("apps/shop/b/http.host", host.clone()),
        ]),
    );
    wait_for_backends(web, "shop.test", &["a", "b"], 20).await;
    // Only key ranges are asked for.
    let requests = etcd.requests();
    assert!(
        requests
            .iter()
            .all(|head| head.starts_with("post /v3/kv/range "))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kubernetes_ingresses_and_http_routes_are_routed_to_ready_endpoints() {
    use serde_json::{Value, json};

    let api = rxh_test_util::CatalogApi::start().await;
    let (a, b) = (backend("a").await, backend("b").await);
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();

    let list = |items: Value| json!({ "items": items }).to_string();
    let ingress = |name: &str, class: &str, host: &str| -> Value {
        json!({
            "metadata": { "name": name, "namespace": "default" },
            "spec": { "ingressClassName": class, "rules": [{ "host": host, "http": { "paths": [{
                "path": "/", "pathType": "Prefix",
                "backend": { "service": { "name": "web", "port": { "number": 80 } } },
            }] } }] },
        })
    };
    // The backends listen on different ports, so each has its own slice.
    let slice = |backend: SocketAddr, ready: bool| -> Value {
        json!({
            "metadata": {
                "name": format!("web-{}", backend.port()), "namespace": "default",
                "labels": { "kubernetes.io/service-name": "web" },
            },
            "ports": [{ "port": backend.port(), "protocol": "TCP" }],
            "endpoints": [{
                "addresses": [backend.ip().to_string()], "conditions": { "ready": ready },
            }],
        })
    };
    let ingresses = "GET /apis/networking.k8s.io/v1/ingresses";
    let slices = "GET /apis/discovery.k8s.io/v1/endpointslices";
    api.publish(
        ingresses,
        &list(json!([
            ingress("shop", "rxh", "shop.test"),
            ingress("other", "nginx", "other.test"),
        ])),
    );
    api.publish(
        "GET /api/v1/services",
        &list(json!([{
            "metadata": { "name": "web", "namespace": "default" },
            "spec": { "ports": [{ "port": 80, "targetPort": 8080 }] },
        }])),
    );
    api.publish(slices, &list(json!([slice(a, true)])));

    // No HTTPRoute list: the API answers 404, like without the Gateway API.
    let _daemon = start_with_catalog(
        dir,
        &format!(
            "[kubernetes]\nendpoint = \"{}\"\ntoken = \"s3cret\"\ningress_class = \"rxh\"\n\
             listen = \"{web}\"\nrefresh = \"50ms\"\n",
            api.endpoint()
        ),
    );
    wait_for_backends(web, "shop.test", &["a"], 1).await;
    assert!(
        api.requests()
            .iter()
            .all(|head| head.contains("authorization: bearer s3cret")),
        "{:?}",
        api.requests()
    );
    // The Ingress of another class is left to its controller.
    assert_ne!(get_host(web, "other.test").await.as_deref(), Some("a"));

    // A second ready endpoint joins without rebuilding the listeners.
    api.publish(slices, &list(json!([slice(a, true), slice(b, true)])));
    wait_for_backends(web, "shop.test", &["a", "b"], 20).await;
    // An endpoint that is not ready gets no requests.
    api.publish(slices, &list(json!([slice(a, false), slice(b, true)])));
    wait_for_backends(web, "shop.test", &["b"], 20).await;
    assert_eq!(log_count(dir, "Kubernetes objects changed"), 1);

    // An HTTPRoute adds its hostname to the pool of the Service port.
    api.publish(
        "GET /apis/gateway.networking.k8s.io/v1/httproutes",
        &list(json!([{
            "metadata": { "name": "route", "namespace": "default" },
            "spec": {
                "hostnames": ["route.test"],
                "rules": [{ "backendRefs": [{ "name": "web", "port": 80 }] }],
            },
        }])),
    );
    wait_for_backends(web, "route.test", &["b"], 1).await;
    wait_for_backends(web, "shop.test", &["b"], 1).await;

    // A list that cannot be read keeps what was last listed.
    api.publish(ingresses, "<html>");
    tokio::time::sleep(Duration::from_millis(300)).await;
    wait_for_backends(web, "shop.test", &["b"], 20).await;

    // Deleted, the Ingress goes away with its host; the HTTPRoute stays.
    api.publish(ingresses, &list(json!([])));
    for _ in 0..250 {
        if get_host(web, "shop.test").await.as_deref() != Some("b") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_ne!(get_host(web, "shop.test").await.as_deref(), Some("b"));
    wait_for_backends(web, "route.test", &["b"], 1).await;
}

/// A backend answering every request with `500 Internal Server Error`.
async fn failing_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::task::spawn(async move {
                let service = service_fn(|_| async {
                    let mut response = Response::new(Full::<Bytes>::from("broken"));
                    *response.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                    Ok::<_, Infallible>(response)
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn top_shows_live_rates_per_pool() {
    let live = backend("live").await;
    let broken = failing_backend().await;
    let (admin, web, bad) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\n\
             [upstream.api]\nbackends = [\"{live}\"]\n\n\
             [upstream.broken]\nbackends = [\"{broken}\"]\n\n\
             [upstream.idle]\nbackends = [\"{}\"]\n\n\
             [[server]]\nlisten = \"{web}\"\nforward = \"@api\"\n\n\
             [[server]]\nlisten = \"{bad}\"\nforward = \"@broken\"\n",
            free_address()
        ),
    )
    .expect("write config");
    let _daemon = start_daemon(dir);
    wait_for_body(web, "live").await;

    let load = tokio::task::spawn(async move {
        loop {
            try_get(web).await;
            try_get(bad).await;
        }
    });
    let output = tokio::task::spawn_blocking({
        let dir = dir.to_owned();
        move || rxh(&dir, admin, &["top", "--interval", "300ms", "-n", "2"])
    })
    .await
    .expect("top runs");
    load.abort();
    let table = assert_success(&output);

    // Not a terminal: two frames, no escape codes.
    assert_eq!(table.matches("rxh top  admin ").count(), 2, "{table}");
    assert!(!table.contains('\x1b'), "{table}");
    let last_frame = table.rsplit("rxh top").next().expect("a frame");
    let row = |pool: &str| -> Vec<String> {
        last_frame
            .lines()
            .find(|line| line.starts_with(&format!("{pool} ")))
            .unwrap_or_else(|| panic!("no row for {pool} in\n{table}"))
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    };
    let rate = |cell: &str| -> f64 { cell.parse().expect("a rate") };

    let api = row("api");
    assert!(rate(&api[1]) > 0.0, "{table}");
    assert_eq!(
        (api[2].as_str(), api[3].as_str()),
        ("0.0", "0.0"),
        "{table}"
    );
    assert!(api[4].ends_with("ms"), "{table}");
    assert_eq!(api[6], "1/1", "{table}");

    let broken_row = row("broken");
    assert!(rate(&broken_row[2]) > 0.0, "{table}");
    assert_eq!(broken_row[3], "100.0", "{table}");

    // An idle pool has no rates to show; its dead backend has no active
    // health check, so it still counts as healthy.
    assert_eq!(row("idle")[1..5], ["0.0", "0.0", "-", "-"], "{table}");
}

#[test]
fn top_fails_without_a_daemon() {
    let directory = tempfile::tempdir().expect("temp dir");
    let stderr = assert_failure(&rxh(directory.path(), free_address(), &["top", "-n", "1"]));
    assert!(stderr.contains("daemon is not reachable"), "{stderr}");
}

/// PID the admin API reports.
#[cfg(target_os = "linux")]
fn daemon_pid(dir: &Path, admin: &str) -> u32 {
    let output = Command::new(RXH)
        .current_dir(dir)
        .args(["--config", "rxh.toml", "--admin", admin, "status", "--json"])
        .output()
        .expect("rxh runs");
    let status: serde_json::Value =
        serde_json::from_str(&assert_success(&output)).expect("status is JSON");
    u32::try_from(status["pid"].as_u64().expect("pid")).expect("pid fits")
}

#[cfg(target_os = "linux")]
async fn wait_for_exit(pid: u32) {
    for _ in 0..500 {
        // A zombie (the test is the parent of the first daemon) has exited.
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if stat.is_empty() || stat.contains(") Z ") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("process {pid} never exited");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_hands_the_listening_sockets_to_a_new_process() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::io::AsyncWriteExt;

    let upstream = backend("backend-1").await;
    let admin = free_address();
    let web = free_address();
    let datagrams = free_address();
    // A backend that answers when the test says so.
    let slow = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let slow_upstream = slow.local_addr().expect("address");
    let slow_web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let config = format!(
        "[admin]\nlisten = \"{admin}\"\n\
         [[server]]\nlisten = \"{web}\"\nforward = \"{upstream}\"\n\
         [[server]]\nlisten = \"{slow_web}\"\nforward = \"{slow_upstream}\"\n\
         [[server]]\nprotocol = \"udp\"\nlisten = \"{datagrams}\"\nforward = \"127.0.0.1:9\"\n"
    );
    std::fs::write(dir.join("rxh.toml"), &config).expect("write");
    let daemon = start_daemon(dir);
    wait_for_body(web, "backend-1").await;
    let first = daemon.0.id();
    assert_eq!(daemon_pid(dir, &admin.to_string()), first);

    // New connections all the time: none may be refused or reset while the
    // processes change.
    let done = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicUsize::new(0));
    let load = {
        let (done, served) = (Arc::clone(&done), Arc::clone(&served));
        tokio::task::spawn(async move {
            while !done.load(Ordering::Relaxed) {
                assert_eq!(
                    try_get(web).await.as_deref(),
                    Some("backend-1"),
                    "request failed during the upgrade"
                );
                served.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // A new process that cannot start leaves the old one serving.
    std::fs::write(dir.join("rxh.toml"), "[[server]]\nlisten = 1").expect("write");
    let stderr = assert_failure(&rxh(dir, admin, &["upgrade"]));
    assert!(
        stderr.contains("the new process stopped before it was ready"),
        "{stderr}"
    );
    assert_eq!(daemon_pid(dir, &admin.to_string()), first);

    // A request the old process is in the middle of: it must finish there.
    let pending = tokio::task::spawn(try_get(slow_web));
    let (mut held, _) = slow.accept().await.expect("request reaches the backend");

    std::fs::write(dir.join("rxh.toml"), &config).expect("write");
    let upgraded = assert_success(&rxh(dir, admin, &["upgrade"]));
    assert!(
        upgraded.contains(&format!("took over from {first}")),
        "{upgraded}"
    );
    // The old process drains, and only the new one answers the admin API.
    for _ in 0..20 {
        assert_ne!(daemon_pid(dir, &admin.to_string()), first);
    }
    let second = daemon_pid(dir, &admin.to_string());
    let _guard = Background(Some(second));
    assert!(is_alive(first), "the old process waits for its request");
    held.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow")
        .await
        .expect("write");
    assert_eq!(pending.await.expect("joins").as_deref(), Some("slow"));
    wait_for_exit(first).await;

    // The new process can be upgraded in turn, by signal.
    let killed = Command::new("kill")
        .args(["-USR2", &second.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    wait_for_exit(second).await;
    let third = daemon_pid(dir, &admin.to_string());
    let _guard = Background(Some(third));
    assert!(third != first && third != second);

    let before = served.load(Ordering::Relaxed);
    wait_for_body(web, "backend-1").await;
    done.store(true, Ordering::Relaxed);
    load.await.expect("no request failed");
    assert!(before > 0, "requests were served during the upgrades");

    assert_success(&rxh(dir, admin, &["stop"]));
    wait_for_exit(third).await;
    assert!(try_get(web).await.is_none(), "listener closed");
    // The UDP port, which cannot be bound twice, was handed over and is
    // free again.
    std::net::UdpSocket::bind(datagrams).expect("UDP port released");
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_of_a_background_daemon_keeps_its_pid_file_and_admin_socket() {
    let upstream = backend("backend-1").await;
    let web = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let admin = format!("unix/{}/admin.sock", dir.display());
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nlisten = \"{web}\"\nforward = \"{upstream}\"\n"
        ),
    )
    .expect("write");
    let cli = |args: &[&str]| {
        Command::new(RXH)
            .current_dir(dir)
            .args(["--config", "rxh.toml", "--pid-file", "rxh.pid"])
            .args(args)
            .output()
            .expect("rxh runs")
    };
    let pid_file = || -> u32 {
        std::fs::read_to_string(dir.join("rxh.pid"))
            .expect("PID file")
            .trim()
            .parse()
            .expect("numeric PID")
    };

    assert_success(&cli(&["start"]));
    let first = pid_file();
    let _guard = Background(Some(first));
    wait_for_body(web, "backend-1").await;

    assert_success(&cli(&["upgrade"]));
    wait_for_exit(first).await;
    let second = daemon_pid(dir, &admin);
    let _guard = Background(Some(second));
    assert_ne!(second, first);
    // The new process waits for the lock of the old one.
    for _ in 0..250 {
        if pid_file() == second {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(pid_file(), second);
    assert_eq!(try_get(web).await.as_deref(), Some("backend-1"));
    let stderr = assert_failure(&cli(&["start"]));
    assert!(stderr.contains("is rxh already running?"), "{stderr}");

    assert_success(&cli(&["stop"]));
    wait_for_exit(second).await;
}

/// With `[admin] socket`, the CLI reaches the daemon there, and the daemon
/// writes the configuration and the users file: the CLI never needs to
/// write them itself.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn edits_go_through_the_daemon_over_the_admin_socket() {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;

    let directory = tempfile::tempdir().expect("temp dir");
    let dir = directory.path();
    let admin = free_address();
    let socket = dir.join("rxh.sock");
    std::fs::write(
        dir.join("rxh.toml"),
        format!(
            "[admin]\nlisten = \"{admin}\"\nsocket = \"{}\"\n",
            socket.display()
        ),
    )
    .expect("write config");
    let mut daemon = Daemon(
        Command::new(RXH)
            .current_dir(dir)
            .env_remove("RXH_ADMIN")
            .env_remove("RXH_ADMIN_TOKEN")
            .env_remove("RXH_CONFIG")
            .args(["run", "--config", "rxh.toml"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon starts"),
    );
    let (lines, log) = mpsc::channel();
    let output = BufReader::new(daemon.0.stdout.take().expect("stdout"));
    std::thread::spawn(move || {
        for line in output.lines().map_while(Result::ok) {
            let _ = lines.send(line);
        }
    });
    let mut seen: Vec<String> = Vec::new();
    let mut wait_for = |text: &str| {
        while !seen.iter().any(|line| line.contains(text)) {
            let line = log
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| panic!("the log says {text:?}, it said {seen:#?}"));
            seen.push(line);
        }
    };
    wait_for("API listening on unix socket");
    wait_for(&format!("Dashboard on http://{admin}/dashboard"));

    // No `--admin`: the CLI finds the socket in the configuration file.
    let status = assert_success(&rxh_bare(dir, &["status"]));
    assert!(status.contains(&format!("admin {admin}")), "{status}");

    let output = rxh_bare(dir, &["pool", "add", "api", "--backend", "127.0.0.1:9"]);
    let printed = assert_success(&output);
    assert!(printed.contains("Created pool api in "), "{printed}");
    wait_for("Admin => Created pool api in ");
    let config = std::fs::read_to_string(dir.join("rxh.toml")).expect("read");
    assert!(config.contains("[upstream.api]"), "{config}");

    let mut add = Command::new(RXH)
        .current_dir(dir)
        .env_remove("RXH_ADMIN")
        .env_remove("RXH_ADMIN_TOKEN")
        .env_remove("RXH_CONFIG")
        .args(["user", "add", "ana"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("rxh runs");
    add.stdin
        .take()
        .expect("stdin")
        .write_all(b"password-ana\n")
        .expect("write");
    assert_success(&add.wait_with_output().expect("rxh ends"));
    wait_for("Admin => User ana created by local");
    let users = assert_success(&rxh_bare(dir, &["user", "list"]));
    assert_eq!(users, "ana admin\n");
}

/// A user who may not open the admin socket is told how to get access,
/// instead of a failed write of the files.
#[cfg(unix)]
#[test]
fn a_socket_this_user_may_not_open_says_how_to_get_access() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temp dir");
    let socket = directory.path().join("rxh.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
        eprintln!("skipped: this user opens any socket (root)");
        return;
    }
    let admin = format!("unix/{}", socket.display());
    let output = rxh_bare(
        directory.path(),
        &[
            "--admin",
            &admin,
            "pool",
            "add",
            "api",
            "--backend",
            "127.0.0.1:9",
        ],
    );
    let stderr = assert_failure(&output);
    assert!(stderr.contains("cannot open the admin socket"), "{stderr}");
    assert!(stderr.contains("usermod -aG rxh"), "{stderr}");
    assert!(!directory.path().join("rxh.toml").exists(), "{stderr}");
}
