//! Without `[admin] token` a local connection to the admin API is an
//! administrator, so the proxy must not open one for its clients: no backend
//! address, health check or TCP route reaches the admin API of the process.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Method, Request, StatusCode, header};
use rxh_server::Daemon;
use rxh_test_util::free_address;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use util::admin::{get, send, start};

const TOKEN: &str = "s3cret-admin-token-0123";

/// Starts a daemon on `config` behind the admin API at `admin`.
async fn daemon(
    directory: &Path,
    admin: SocketAddr,
    config: &str,
) -> JoinHandle<Result<(), rxh_server::Error>> {
    let path = directory.join("rxh.toml");
    fs::write(&path, config).expect("write config");
    start(&path, admin).await
}

/// `GET uri` through `proxy` with the `Host` the admin API takes for local,
/// and maybe the token.
async fn through(
    proxy: SocketAddr,
    admin: SocketAddr,
    uri: &str,
    bearer: Option<&str>,
) -> StatusCode {
    let mut request = Request::get(uri).header(header::HOST, admin.to_string());
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = request.body(Empty::<Bytes>::new()).expect("valid request");
    util::send(proxy, request).await.0
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_forwarding_to_the_admin_api_gets_502_without_a_token() {
    let (admin, by_address, by_name) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let port = admin.port();
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nlisten = \"{by_address}\"\nforward = \"127.0.0.1:{port}\"\n\
             [[server]]\nlisten = \"{by_name}\"\nforward = \"localhost:{port}\"\n"
        ),
    )
    .await;

    // What the proxy would be given, asked directly.
    let (status, _) = get(admin, "/config/files").await;
    assert_eq!(status, StatusCode::OK);

    for proxy in [by_address, by_name] {
        let status = through(proxy, admin, "/config/files", None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "through {proxy}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tcp_stream_to_the_admin_api_is_refused_without_a_token() {
    let (admin, proxy) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nlisten = \"{proxy}\"\nprotocol = \"tcp\"\nforward = \"{admin}\"\n"
        ),
    )
    .await;

    let mut stream = TcpStream::connect(proxy).await.expect("proxy accepts");
    let request = format!("GET /status HTTP/1.1\r\nHost: {admin}\r\nConnection: close\r\n\r\n");
    // The proxy may have closed already.
    let _ = stream.write_all(request.as_bytes()).await;
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).await;
    assert!(
        answer.is_empty(),
        "the admin API answered: {}",
        String::from_utf8_lossy(&answer)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_health_check_never_reaches_the_admin_api_without_a_token() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    // `/status` answers 200 to a local request: a check that reached the
    // admin API would pass.
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [upstream.api]\nbackends = [\"{admin}\"]\n\
             health = {{ uri = \"/status\", interval = \"20ms\", timeout = \"1s\" }}\n"
        ),
    )
    .await;

    let healthy = async || get(admin, "/pools/api").await.1["upstreams"][0]["healthy"] == true;
    util::eventually(async || !healthy().await).await;
    // Many checks later, none has passed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!healthy().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn with_a_token_the_admin_api_can_be_published() {
    let (admin, proxy) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!(
            "[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n\
             [[server]]\nlisten = \"{proxy}\"\nforward = \"{admin}\"\n"
        ),
    )
    .await;

    let status = through(proxy, admin, "/config/files", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let status = through(proxy, admin, "/config/files", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn other_local_services_are_still_reachable() {
    let backend = util::backend("backend").await;
    let (admin, by_address, by_name) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let port = backend.port();
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [[server]]\nlisten = \"{by_address}\"\nforward = \"{backend}\"\n\
             [[server]]\nlisten = \"{by_name}\"\nforward = \"localhost:{port}\"\n"
        ),
    )
    .await;

    for proxy in [by_address, by_name] {
        let (status, _, body) = util::get(proxy, "/").await;
        assert_eq!(status, StatusCode::OK, "through {proxy}");
        assert_eq!(&body[..], b"backend");
    }
}

/// `[admin]` is read at start: a reload that writes a token does not give
/// the listening admin API one, so the proxy keeps away from it.
#[tokio::test(flavor = "multi_thread")]
async fn a_reload_that_adds_a_token_does_not_open_the_way() {
    let (admin, proxy) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let server = format!("[[server]]\nlisten = \"{proxy}\"\nforward = \"{admin}\"\n");
    let _daemon = daemon(
        directory.path(),
        admin,
        &format!("[admin]\nlisten = \"{admin}\"\n{server}"),
    )
    .await;

    fs::write(
        directory.path().join("rxh.toml"),
        format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n{server}# Reloaded.\n"),
    )
    .expect("write config");
    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // Still open to a local request without the token, so still refused.
    let (status, _) = get(admin, "/config/files").await;
    assert_eq!(status, StatusCode::OK);
    let status = through(proxy, admin, "/config/files", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_host_forwarding_to_the_admin_socket_gets_502_without_a_token() {
    let proxy = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let socket = directory.path().join("admin.sock");
    let link = directory.path().join("link.sock");
    std::os::unix::fs::symlink(&socket, &link).expect("symlink");
    let config = directory.path().join("rxh.toml");
    fs::write(
        &config,
        format!(
            "[admin]\nlisten = \"unix/{}\"\n\
             [[server]]\nlisten = \"{proxy}\"\nforward = \"unix/{}\"\n",
            socket.display(),
            link.display()
        ),
    )
    .expect("write config");
    let _daemon = tokio::task::spawn(Daemon::new(&config).run());
    util::eventually(async || UnixStream::connect(&socket).await.is_ok()).await;
    util::eventually(async || TcpStream::connect(proxy).await.is_ok()).await;

    let request = Request::get("/config/files")
        .header(header::HOST, "localhost")
        .body(Empty::<Bytes>::new())
        .expect("valid request");
    let (status, _, _) = util::send(proxy, request).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
