//! `rxh user`: the accounts of the admin API, edited in the users file.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::access::Access;
use rxh_test_util::free_address;
use tokio::net::TcpStream;

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

const TOKEN: &str = "0123456789abcdef";

/// A directory with an `rxh.toml` whose admin API listens on `admin` with a
/// token, so that a local request without a cookie is nobody.
fn directory(admin: SocketAddr) -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let config = format!(
        "[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"127.0.0.1:1\"\n"
    );
    std::fs::write(directory.path().join("rxh.toml"), config).unwrap();
    directory
}

/// Runs `rxh user ARGS` in `directory` with `input` on standard input.
fn user(directory: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(RXH)
        .current_dir(directory)
        .arg("user")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn succeeds(output: &Output) -> String {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn fails(output: &Output) -> String {
    assert!(!output.status.success(), "the command should fail");
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// The users file at its default place.
fn users(directory: &Path) -> Access {
    Access::load(&directory.join("rxh-data/users.toml")).unwrap()
}

#[test]
fn user_add_reads_the_password_from_standard_input_and_list_shows_it() {
    let directory = directory(free_address());
    let dir = directory.path();
    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    succeeds(&user(
        dir,
        &["add", "luis", "--role", "admin"],
        "password-luis\n",
    ));
    succeeds(&user(dir, &["add", "eva"], "password-eva\n"));

    assert!(users(dir).verify("ana", "password-ana"));
    assert!(!users(dir).verify("ana", "password-ana\n"));
    let listed = succeeds(&user(dir, &["list"], ""));
    assert_eq!(listed, "ana admin\nluis admin\neva\n");

    let stderr = fails(&user(dir, &["add", "ana"], "password-ana\n"));
    assert_eq!(stderr, "rxh: user \"ana\" exists\n");
    let stderr = fails(&user(
        dir,
        &["add", "carla", "--role", "missing"],
        "password-carla\n",
    ));
    assert!(stderr.contains("missing"), "{stderr}");
    let stderr = fails(&user(dir, &["add", "carla"], ""));
    assert_eq!(stderr, "rxh: no password on standard input\n");
    assert_eq!(users(dir).user.len(), 3);
}

#[test]
fn the_first_user_is_an_administrator() {
    let directory = directory(free_address());
    let dir = directory.path();
    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    succeeds(&user(dir, &["add", "eva"], "password-eva\n"));
    let access = users(dir);
    assert_eq!(access.user[0].roles, ["admin"]);
    assert!(access.user[1].roles.is_empty(), "only the first one");
}

#[test]
fn passwd_changes_the_password() {
    let directory = directory(free_address());
    let dir = directory.path();
    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    succeeds(&user(dir, &["passwd", "ana"], "another-password\n"));
    assert!(users(dir).verify("ana", "another-password"));
    assert!(!users(dir).verify("ana", "password-ana"));

    let stderr = fails(&user(dir, &["passwd", "eva"], "password-eva\n"));
    assert_eq!(stderr, "rxh: no user \"eva\"\n");
}

#[test]
fn remove_refuses_the_last_administrator() {
    let directory = directory(free_address());
    let dir = directory.path();
    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    succeeds(&user(dir, &["add", "eva"], "password-eva\n"));

    let stderr = fails(&user(dir, &["remove", "ana"], ""));
    assert_eq!(stderr, "rxh: \"ana\" is the last administrator\n");
    succeeds(&user(dir, &["remove", "eva"], ""));
    assert_eq!(succeeds(&user(dir, &["list"], "")), "ana admin\n");
    let stderr = fails(&user(dir, &["remove", "eva"], ""));
    assert_eq!(stderr, "rxh: no user \"eva\"\n");
}

#[test]
fn a_short_password_is_rejected() {
    let directory = directory(free_address());
    let dir = directory.path();
    let stderr = fails(&user(dir, &["add", "ana"], "short\n"));
    assert!(
        stderr.starts_with("rxh: ") && stderr.contains('8'),
        "{stderr}"
    );
    assert!(users(dir).user.is_empty());

    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    fails(&user(dir, &["passwd", "ana"], "short\n"));
    assert!(users(dir).verify("ana", "password-ana"));
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts `rxh run` in `directory` and waits for its admin API.
async fn start_daemon(directory: &Path, admin: SocketAddr) -> Daemon {
    start_daemon_with(directory, &[], admin).await
}

/// Starts `rxh run ARGS` in `directory` and waits for its admin API.
async fn start_daemon_with(directory: &Path, args: &[&str], admin: SocketAddr) -> Daemon {
    let daemon = Daemon(
        Command::new(RXH)
            .current_dir(directory)
            .arg("run")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..250 {
        if TcpStream::connect(admin).await.is_ok() {
            return daemon;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the admin API never listened on {admin}");
}

/// Sends one request to the admin API: status and the `Set-Cookie` header.
async fn call(
    admin: SocketAddr,
    method: Method,
    uri: &str,
    body: String,
    cookie: Option<&str>,
) -> (StatusCode, Option<String>) {
    let stream = TcpStream::connect(admin).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, admin.to_string());
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let request = request.body(Full::new(Bytes::from(body))).unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    let cookie = response.headers().get(header::SET_COOKIE).map(|value| {
        value
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    });
    let _ = response.into_body().collect().await.unwrap();
    (status, cookie)
}

/// Signs in: status and the `name=value` of the session cookie.
async fn sign_in(admin: SocketAddr, name: &str, password: &str) -> (StatusCode, Option<String>) {
    let body = serde_json::json!({ "name": name, "password": password }).to_string();
    call(admin, Method::POST, "/auth/login", body, None).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_added_with_the_cli_signs_in_to_a_running_daemon() {
    let admin = free_address();
    let directory = directory(admin);
    let dir = directory.path();
    let _daemon = start_daemon(dir, admin).await;

    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    let (status, cookie) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cookie = cookie.expect("a session cookie");
    let (status, _) = call(admin, Method::GET, "/users", String::new(), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "ana is an administrator");
}

#[tokio::test(flavor = "multi_thread")]
async fn passwd_ends_the_sessions_of_a_running_daemon() {
    let admin = free_address();
    let directory = directory(admin);
    let dir = directory.path();
    succeeds(&user(dir, &["add", "ana"], "password-ana\n"));
    let _daemon = start_daemon(dir, admin).await;

    let (status, cookie) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cookie = cookie.expect("a session cookie");
    let (status, _) = call(admin, Method::GET, "/users", String::new(), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);

    succeeds(&user(dir, &["passwd", "ana"], "another-password\n"));
    let (status, _) = call(admin, Method::GET, "/users", String::new(), Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "the old session ended");
    let (status, _) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, cookie) = sign_in(admin, "ana", "another-password").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cookie = cookie.expect("a session cookie");
    let (status, _) = call(admin, Method::GET, "/users", String::new(), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_users_file_is_next_to_the_configuration_whatever_the_working_directory() {
    let admin = free_address();
    let directory = directory(admin);
    let config = directory.path().join("rxh.toml");
    let config = config.to_str().unwrap();
    let (daemon_cwd, cli_cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let _daemon = start_daemon_with(daemon_cwd.path(), &["-c", config], admin).await;

    succeeds(&user(
        cli_cwd.path(),
        &["-c", config, "add", "ana"],
        "password-ana\n",
    ));
    assert!(users(directory.path()).verify("ana", "password-ana"));
    assert!(!cli_cwd.path().join("rxh-data").exists());
    let (status, _) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!daemon_cwd.path().join("rxh-data").exists());
}
