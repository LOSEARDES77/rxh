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
