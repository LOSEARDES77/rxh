//! Admin API routes, reloads through it (including the rollback when a new
//! listener cannot bind) and `/stop`.

mod util;

use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::Path;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::Daemon;
use rxh_server::config::Config;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address")
}

/// Sends one request to the admin API and returns status, headers and the
/// body parsed as JSON (`Value::Null` when it is not JSON).
async fn call(
    admin: SocketAddr,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let stream = TcpStream::connect(admin).await.expect("admin accepts");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let mut request = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(name, _)| name == header::HOST) {
        request = request.header(header::HOST, admin.to_string());
    }
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    let request = request
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("valid request");
    let response = sender.send_request(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (parts.status, parts.headers, json)
}

async fn get(admin: SocketAddr, uri: &str) -> (StatusCode, Value) {
    let (status, _, json) = call(admin, Method::GET, uri, "", &[]).await;
    (status, json)
}

async fn send(admin: SocketAddr, method: Method, uri: &str, body: &str) -> (StatusCode, Value) {
    let (status, _, json) = call(admin, method, uri, body, &[]).await;
    (status, json)
}

fn error_text(json: &Value) -> &str {
    json["error"].as_str().expect("error message")
}

fn addresses(pool: &Value) -> Vec<&str> {
    pool["upstreams"]
        .as_array()
        .expect("upstream list")
        .iter()
        .map(|upstream| upstream["address"].as_str().expect("address"))
        .collect()
}

/// Starts a daemon on the configuration file and waits for its admin API.
async fn start(config_path: &Path, admin: SocketAddr) -> JoinHandle<Result<(), rxh_server::Error>> {
    let daemon = tokio::task::spawn(Daemon::new(config_path).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;
    daemon
}

async fn body_of(address: SocketAddr) -> String {
    let (_, _, body) = util::get(address, "/").await;
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_answer_with_status_codes_and_json() {
    let first = util::backend("first").await;
    let second = util::backend("second").await;
    let (admin, web) = (free_port(), free_port());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [upstream.api]\nbackends = [\"{first}\"]\n\
             [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"@api\"\n"
        ),
    )
    .expect("write config");
    let _daemon = start(&config_path, admin).await;

    // Status, on `/` and `/status`.
    for uri in ["/", "/status"] {
        let (status, json) = get(admin, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(json["version"], rxh_server::VERSION);
        assert_eq!(json["pid"], std::process::id());
        assert_eq!(json["admin"], admin.to_string());
        assert!(
            json["config_path"]
                .as_str()
                .expect("path")
                .ends_with("rxh.toml")
        );
        assert_eq!(json["servers"][0]["name"], "web");
        assert_eq!(json["servers"][0]["protocol"], "http");
        assert_eq!(json["servers"][0]["state"], "listening");
        assert_eq!(json["servers"][0]["listen"], web.to_string());
        assert_eq!(json["pools"][0]["name"], "api");
    }

    // Pools and the Caddy compatible upstream list.
    let (status, pools) = get(admin, "/pools").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pools.as_array().expect("array").len(), 1);
    let (status, pool) = get(admin, "/pools/api").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(addresses(&pool), [first.to_string()]);
    let (status, json) = get(admin, "/pools/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&json), "no pool named nope");
    // Path segments are percent-decoded.
    let (status, json) = get(admin, "/pools/a%20b").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&json), "no pool named a b");
    let (status, upstreams) = get(admin, "/reverse_proxy/upstreams").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstreams[0]["address"], first.to_string());
    assert_eq!(upstreams[0]["fails"], 0);

    // Metrics are Prometheus text, not JSON.
    let (status, headers, _) = call(admin, Method::GET, "/metrics", "", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; version=0.0.4");

    // Adding: default weight, conflicts, validation and unknown pools.
    let add = format!("{{\"address\": \"{second}\"}}");
    let (status, pool) = send(admin, Method::POST, "/pools/api/upstreams", &add).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(addresses(&pool), [first.to_string(), second.to_string()]);
    assert_eq!(pool["upstreams"][1]["weight"], 1);
    let (status, json) = send(admin, Method::PUT, "/pools/api/upstreams", &add).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    let mixed = r#"{"address": "https://127.0.0.1:1"}"#;
    let (status, json) = send(admin, Method::POST, "/pools/api/upstreams", mixed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_text(&json).contains("schemes can't be mixed"),
        "{json}"
    );
    let (status, json) = send(admin, Method::POST, "/pools/api/upstreams", "{oops").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_text(&json).starts_with("invalid JSON body"), "{json}");
    let huge = format!("{{\"address\": \"{}\"}}", "x".repeat(70 * 1024));
    let (status, json) = send(admin, Method::POST, "/pools/api/upstreams", &huge).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_text(&json).starts_with("could not read body"),
        "{json}"
    );
    let (status, json) = send(admin, Method::POST, "/pools/nope/upstreams", &add).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&json), "no pool named nope");
    util::eventually(async || body_of(web).await == "second").await;

    // The live configuration shows the added backend.
    let (status, config) = get(admin, "/config").await;
    assert_eq!(status, StatusCode::OK);
    let backends = config["upstream"]["api"]["backends"].to_string();
    assert!(backends.contains(&second.to_string()), "{config}");

    // Removing: by query, by JSON body, unknown members and pools.
    let by_query = format!("/pools/api/upstreams?address={second}");
    let (status, pool) = send(admin, Method::DELETE, &by_query, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(addresses(&pool), [first.to_string()]);
    let (status, json) = send(admin, Method::DELETE, &by_query, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&json), format!("{second} is not in pool api"));
    let (status, json) = send(admin, Method::DELETE, "/pools/api/upstreams", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no address at all");
    assert!(error_text(&json).starts_with("invalid JSON body"), "{json}");
    let by_body = format!("{{\"address\": \"{first}\"}}");
    let (status, json) = send(admin, Method::DELETE, "/pools/nope/upstreams", &by_body).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{json}");
    let (status, pool) = send(admin, Method::DELETE, "/pools/api/upstreams", &by_body).await;
    assert_eq!(status, StatusCode::OK);
    assert!(addresses(&pool).is_empty());

    // Unknown routes and methods.
    let (status, json) = get(admin, "/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_text(&json), "no route for GET /nope");
    let (status, _) = send(admin, Method::POST, "/status", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_host_or_origin_is_forbidden() {
    let admin = free_port();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(&config_path, format!("[admin]\nlisten = \"{admin}\"\n")).expect("write");
    let _daemon = start(&config_path, admin).await;

    // Every route, including the ones that change state, is guarded.
    for (method, uri) in [
        (Method::GET, "/status"),
        (Method::POST, "/stop"),
        (Method::POST, "/reload"),
    ] {
        for headers in [
            [(header::HOST, "evil.example")].as_slice(),
            &[(header::ORIGIN, "http://evil.example")],
            &[(header::ORIGIN, "http://127.0.0.1.evil.example:80/")],
        ] {
            let (status, _, json) = call(admin, method.clone(), uri, "", headers).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri} {headers:?}");
            assert_eq!(
                error_text(&json),
                "admin API only accepts loopback Host and Origin"
            );
        }
    }

    let local = [(header::ORIGIN, "http://localhost:2019/")];
    let (status, _, _) = call(admin, Method::GET, "/status", "", &local).await;
    assert_eq!(status, StatusCode::OK, "the daemon is still running");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_applies_valid_files_and_rejects_broken_ones() {
    let first = util::backend("first").await;
    let second = util::backend("second").await;
    let (admin, web) = (free_port(), free_port());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let config = |backend: SocketAddr| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        )
    };
    fs::write(&config_path, config(first)).expect("write");
    let _daemon = start(&config_path, admin).await;
    assert_eq!(body_of(web).await, "first");

    fs::write(&config_path, config(second)).expect("write");
    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["reloaded"], true);
    util::eventually(async || body_of(web).await == "second").await;

    // Invalid TOML and an invalid configuration: 400, old one keeps serving.
    for broken in [
        String::from("[[server]]\nlisten = 1"),
        format!(
            "[admin]\nlisten = \"{admin}\"\n[[server]]\nlisten = \"{web}\"\nforward = \"@missing\"\n"
        ),
    ] {
        fs::write(&config_path, broken).expect("write");
        let (status, json) = send(admin, Method::POST, "/load", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        assert!(!error_text(&json).is_empty());
        assert_eq!(body_of(web).await, "second");
    }

    // A deleted file is also a rejected reload.
    fs::remove_file(&config_path).expect("remove");
    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error_text(&json).contains("rxh.toml"), "{json}");
    assert_eq!(body_of(web).await, "second");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_reload_after_stopping_the_old_listeners_restores_them() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let echo = util::udp_echo("e:").await;
    let (admin, web) = (free_port(), free_port());
    // UDP listeners bind without SO_REUSEPORT: a busy port fails with
    // `AddrInUse`, which makes the daemon stop the old generation first.
    let squatter = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let busy = squatter.local_addr().expect("address");

    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let web_server = |backend: SocketAddr| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n[[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        )
    };
    fs::write(&config_path, web_server(old)).expect("write");
    let _daemon = start(&config_path, admin).await;
    assert_eq!(body_of(web).await, "old");

    fs::write(
        &config_path,
        format!(
            "{}[[server]]\nlisten = \"{busy}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n",
            web_server(new)
        ),
    )
    .expect("write");
    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!error_text(&json).is_empty());

    // The previous configuration runs again and the admin API reports it.
    util::eventually(async || TcpStream::connect(web).await.is_ok() && body_of(web).await == "old")
        .await;
    let (_, status) = get(admin, "/status").await;
    let servers = status["servers"].as_array().expect("servers");
    assert_eq!(servers.len(), 1, "{status}");
    assert_eq!(servers[0]["name"], "web");
    drop(squatter);
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_listener_is_rebound_on_reload() {
    let one = util::udp_echo("one:").await;
    let two = util::udp_echo("two:").await;
    let admin = free_port();
    let listen = UdpSocket::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address");
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let config = |backend: SocketAddr| {
        format!(
            "[admin]\nlisten = \"{admin}\"\n[[server]]\nlisten = \"{listen}\"\nprotocol = \"udp\"\nforward = \"{backend}\"\n"
        )
    };
    fs::write(&config_path, config(one)).expect("write");
    let _daemon = start(&config_path, admin).await;

    async fn exchange(listen: SocketAddr) -> String {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind");
        socket.send_to(b"hi", listen).await.expect("send");
        let mut buffer = [0; 64];
        match tokio::time::timeout(
            std::time::Duration::from_millis(500),
            socket.recv(&mut buffer),
        )
        .await
        {
            Ok(Ok(read)) => String::from_utf8_lossy(&buffer[..read]).into_owned(),
            _ => String::new(),
        }
    }
    util::eventually(async || exchange(listen).await == "one:hi").await;

    // The same UDP port cannot be bound twice, so this reload goes through
    // the stop-then-start path.
    fs::write(&config_path, config(two)).expect("write");
    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    util::eventually(async || exchange(listen).await == "two:hi").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_the_daemon_and_memory_configs_cannot_reload() {
    let admin = free_port();
    let config = Config::from_toml(&format!("[admin]\nlisten = \"{admin}\"\n")).expect("valid");
    let daemon = tokio::task::spawn(Daemon::from_config(config).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    let (status, json) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_text(&json), "no configuration file to reload");
    let (_, status) = get(admin, "/status").await;
    assert_eq!(status["config_path"], Value::Null);

    let (status, json) = send(admin, Method::POST, "/stop", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["stopping"], true);
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), daemon)
        .await
        .expect("daemon stops")
        .expect("no panic");
    assert!(result.is_ok(), "{result:?}");
    assert!(TcpStream::connect(admin).await.is_err(), "admin API closed");
}

#[tokio::test(flavor = "multi_thread")]
async fn busy_admin_address_fails_startup_with_a_hint() {
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let admin = squatter.local_addr().expect("address");
    let config = Config::from_toml(&format!("[admin]\nlisten = \"{admin}\"\n")).expect("valid");

    let err = Daemon::from_config(config)
        .run()
        .await
        .expect_err("admin address is taken")
        .to_string();
    assert!(err.contains(&format!("admin API on {admin}")), "{err}");
    assert!(err.contains("is another rxh running?"), "{err}");
}
