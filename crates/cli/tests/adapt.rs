//! `rxh adapt`: Caddyfile and nginx files converted, then served by the daemon.

#![allow(clippy::unwrap_used)]

use std::convert::Infallible;
use std::fs;
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

/// A backend answering `tag`.
async fn backend(tag: &'static str) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
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

async fn try_get(address: SocketAddr, path: &str) -> Option<String> {
    let stream = TcpStream::connect(address).await.ok()?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .ok()?;
    tokio::task::spawn(connection);
    let request = hyper::Request::get(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())
        .ok()?;
    let response = sender.send_request(request).await.ok()?;
    let body = response.into_body().collect().await.ok()?.to_bytes();
    Some(String::from_utf8_lossy(&body).into_owned())
}

async fn wait_for_body(address: SocketAddr, path: &str, expected: &str) {
    for _ in 0..250 {
        if try_get(address, path).await.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("{address}{path} never answered {expected:?}");
}

fn adapt(directory: &Path, args: &[&str]) -> Output {
    Command::new(RXH)
        .current_dir(directory)
        .arg("adapt")
        .args(args)
        .env_remove("RXH_CONFIG")
        .output()
        .unwrap()
}

fn stdout_and_stderr(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the daemon on the configuration `toml`.
fn run(directory: &Path, toml: &str) -> Daemon {
    fs::write(directory.join("rxh.toml"), toml).unwrap();
    Daemon(
        Command::new(RXH)
            .current_dir(directory)
            .args(["run", "--config", "rxh.toml"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn converted_caddyfile_is_served() {
    let api = backend("api").await;
    let web = backend("web").await;
    let admin = free_address();
    let listen = free_address();
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    fs::write(
        dir.join("Caddyfile"),
        format!(
            "{{\n\tadmin {admin}\n}}\n\nhttp://{listen} {{\n\t@api path /api/*\n\
             \treverse_proxy @api {api}\n\treverse_proxy {web}\n\ttry_files {{path}}\n}}\n"
        ),
    )
    .unwrap();

    let output = adapt(dir, &["Caddyfile"]);
    let (toml, stderr) = stdout_and_stderr(&output);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(stderr, "rxh: not converted: line 9: directive try_files\n");
    assert!(toml.starts_with("# Converted from Caddyfile by rxh adapt.\n"));
    assert!(toml.contains("# Not converted: line 9: directive try_files\n"));

    let _daemon = run(dir, &toml);
    wait_for_body(listen, "/api/users", "api").await;
    assert_eq!(try_get(listen, "/").await.as_deref(), Some("web"));
}

#[tokio::test(flavor = "multi_thread")]
async fn converted_nginx_configuration_is_served() {
    let first = backend("pool").await;
    let static_backend = backend("static").await;
    let tcp_backend = backend("tcp").await;
    let listen = free_address();
    let tcp = free_address();
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    // No `.conf` name: detected from the `;` at the end of lines.
    fs::write(
        dir.join("site"),
        format!(
            "events {{ worker_connections 64; }}\nhttp {{\n  upstream app {{ server {first}; }}\n\
             server {{\n    listen {listen};\n    location / {{ proxy_pass http://app; }}\n\
             location /static/ {{ proxy_pass http://{static_backend}; }}\n\
             location ~ \\.php$ {{ fastcgi_pass 127.0.0.1:9000; }}\n  }}\n}}\n\
             stream {{\n  server {{ listen {tcp}; proxy_pass {tcp_backend}; }}\n}}\n"
        ),
    )
    .unwrap();

    let output = adapt(dir, &["site"]);
    let (toml, stderr) = stdout_and_stderr(&output);
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        stderr,
        "rxh: not converted: line 8: location ~ \\.php$ (regex and named locations)\n"
    );

    let _daemon = run(dir, &format!("{toml}\n[admin]\nenabled = false\n"));
    wait_for_body(listen, "/", "pool").await;
    assert_eq!(
        try_get(listen, "/static/a.css").await.as_deref(),
        Some("static")
    );
    wait_for_body(tcp, "/", "tcp").await;
}

#[test]
fn adapter_flag_overrides_the_file_name() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    fs::write(
        dir.join("Caddyfile"),
        "server { listen 8080; return 301 https://a.b; }\n",
    )
    .unwrap();

    let output = adapt(dir, &["Caddyfile"]);
    let (_, stderr) = stdout_and_stderr(&output);
    assert!(
        !output.status.success(),
        "parsed as a Caddyfile: {output:?}"
    );
    assert!(stderr.contains("directive listen"), "{stderr}");
    assert!(
        stderr.ends_with("rxh: nothing could be converted\n"),
        "{stderr}"
    );

    let output = adapt(dir, &["--adapter", "nginx", "Caddyfile"]);
    let (toml, stderr) = stdout_and_stderr(&output);
    assert!(output.status.success(), "{stderr}");
    assert!(toml.contains("listen = \"0.0.0.0:8080\""), "{toml}");
    assert!(
        toml.contains("redirect = { to = \"https://a.b\", status = 301 }"),
        "{toml}"
    );
}

#[test]
fn failures_name_the_file_and_the_problem() {
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();

    let output = adapt(dir, &["missing.conf"]);
    let (_, stderr) = stdout_and_stderr(&output);
    assert!(!output.status.success());
    assert!(stderr.contains("cannot read missing.conf"), "{stderr}");

    fs::write(dir.join("Caddyfile"), ":80 {\n\treverse_proxy :3000\n").unwrap();
    let output = adapt(dir, &["Caddyfile"]);
    let (stdout, stderr) = stdout_and_stderr(&output);
    assert!(!output.status.success());
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("Caddyfile: line 1: block not closed"),
        "{stderr}"
    );

    // Converted, but RXH rejects the result: printed anyway, with the reason.
    fs::write(
        dir.join("Caddyfile"),
        ":80 {\n\tredir /a /b?m={http.request.method}\n}\n",
    )
    .unwrap();
    let output = adapt(dir, &["Caddyfile"]);
    let (stdout, stderr) = stdout_and_stderr(&output);
    assert!(!output.status.success());
    assert!(
        stdout.contains("redirect = \"/b?m={http.request.method}\""),
        "{stdout}"
    );
    assert!(
        stderr.contains("the converted configuration needs changes by hand")
            && stderr.contains("http.request.method"),
        "{stderr}"
    );
}
