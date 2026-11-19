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
use rxh_test_util::free_address;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

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
    call_on(stream, &admin.to_string(), method, uri, body, headers).await
}

/// [`call`] on an open connection, TCP or unix socket.
async fn call_on<S>(
    stream: S,
    host: &str,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> (StatusCode, HeaderMap, Value)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let mut request = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(name, _)| name == header::HOST) {
        request = request.header(header::HOST, host);
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
    let (admin, web) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [acme.dns]\nprovider = \"rfc2136\"\nserver = \"127.0.0.1\"\n\
             tsig_key = \"rxh.\"\ntsig_secret = \"c2VjcmV0\"\n\
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
    // The secrets of the DNS provider never leave the process.
    assert_eq!(config["acme"]["dns"]["tsig_key"], "rxh.");
    assert_eq!(config["acme"]["dns"]["tsig_secret"], "***");

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
    let admin = free_address();
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
    let (admin, web) = (free_address(), free_address());
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
    let (admin, web) = (free_address(), free_address());
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
    let admin = free_address();
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

    // The same UDP port cannot be bound twice, so each reload goes through
    // the stop-then-start path. The open sessions share the listening socket:
    // the stop must close it with them, or the start finds the port in use.
    for (backend, answer) in [(two, "two:hi"), (one, "one:hi")].repeat(4) {
        let clients: Vec<_> = (0..16).map(|_| tokio::spawn(exchange(listen))).collect();
        for client in clients {
            client.await.expect("client");
        }
        fs::write(&config_path, config(backend)).expect("write");
        let (status, json) = send(admin, Method::POST, "/reload", "").await;
        assert_eq!(status, StatusCode::OK, "{json}");
        util::eventually(async || exchange(listen).await == answer).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_ends_the_daemon_and_memory_configs_cannot_reload() {
    let admin = free_address();
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

#[tokio::test(flavor = "multi_thread")]
async fn load_runs_the_uploaded_configuration_until_the_next_reload() {
    let first = util::backend("first").await;
    let second = util::backend("second").await;
    let (admin, web, other) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let file = format!(
        "[admin]\nlisten = \"{admin}\"\n\
         [acme.dns]\nprovider = \"cloudflare\"\napi_token = \"real-secret\"\n\
         [[server]]\nlisten = \"{web}\"\nforward = \"{first}\"\n"
    );
    fs::write(&config_path, &file).expect("write");
    let _daemon = start(&config_path, admin).await;
    assert_eq!(body_of(web).await, "first");

    // TOML, with an `[admin]` table that would cut the API off: ignored.
    let upload = format!(
        "[admin]\nenabled = false\n[[server]]\nlisten = \"{web}\"\nforward = \"{second}\"\n"
    );
    let (status, json) = send(admin, Method::POST, "/load", &upload).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["loaded"], true);
    util::eventually(async || body_of(web).await == "second").await;
    let (status, config) = get(admin, "/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["admin"]["listen"], admin.to_string());
    assert_eq!(config["acme"]["dns"], Value::Null, "the upload has no DNS");
    assert_eq!(fs::read_to_string(&config_path).expect("read"), file);

    // Rejected uploads leave the running configuration alone.
    let rejected = [
        (String::from("[[server]]\nlisten = 1"), "invalid TOML body"),
        (String::from("{\"server\": 1}"), "invalid JSON body"),
        (String::from("{\"nope\": 1}"), "nope"),
        (String::from("include = [\"conf.d/*.toml\"]\n"), "include"),
        (
            format!("[[server]]\nlisten = \"{web}\"\nforward = \"@missing\"\n"),
            "missing",
        ),
        // A listener that cannot bind: the address is the admin API's.
        (
            format!("[[server]]\nlisten = \"{admin}\"\nforward = \"{first}\"\n"),
            "",
        ),
    ];
    for (body, expected) in rejected {
        let (status, json) = send(admin, Method::POST, "/load", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json}");
        assert!(error_text(&json).contains(expected), "{json}");
        assert_eq!(body_of(web).await, "second");
    }
    let huge = format!("# {}\n", "x".repeat(5 * 1024 * 1024));
    let (status, json) = send(admin, Method::POST, "/load", &huge).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        error_text(&json).starts_with("could not read body"),
        "{json}"
    );

    // Without a body it reloads the file, as `/reload` does.
    let (status, json) = send(admin, Method::POST, "/load", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["reloaded"], true);
    util::eventually(async || body_of(web).await == "first").await;

    // What `GET /config` prints loads again, as JSON, and the redacted
    // secret is the running one afterwards.
    let (_, mut config) = get(admin, "/config").await;
    assert_eq!(config["acme"]["dns"]["api_token"], "***");
    config["server"][0]["listen"] = Value::from(vec![other.to_string()]);
    let (status, json) = send(admin, Method::POST, "/load", &config.to_string()).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    util::eventually(async || TcpStream::connect(web).await.is_err()).await;
    assert_eq!(body_of(other).await, "first");
    let (_, config) = get(admin, "/config").await;
    assert_eq!(config["acme"]["dns"]["api_token"], "***");
}

#[tokio::test(flavor = "multi_thread")]
async fn load_works_without_a_configuration_file() {
    let backend = util::backend("uploaded").await;
    let (admin, web) = (free_address(), free_address());
    let config = Config::from_toml(&format!("[admin]\nlisten = \"{admin}\"\n")).expect("valid");
    let _daemon = tokio::task::spawn(Daemon::from_config(config).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    let upload = format!("[[server]]\nlisten = \"{web}\"\nforward = \"{backend}\"\n");
    let (status, json) = send(admin, Method::POST, "/load", &upload).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(body_of(web).await, "uploaded");
}

#[tokio::test(flavor = "multi_thread")]
async fn patch_changes_one_pool_or_one_server() {
    let first = util::backend("first").await;
    let second = util::backend("second").await;
    let (admin, web, moved) = (free_address(), free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let file = format!(
        "[admin]\nlisten = \"{admin}\"\n\
         [upstream.api]\nbackends = [\"{first}\"]\n\
         [upstream.spare]\nbackends = [\"{second}\"]\n\
         [[server]]\nname = \"web\"\nlisten = \"{web}\"\nforward = \"@api\"\n"
    );
    fs::write(&config_path, &file).expect("write");
    let _daemon = start(&config_path, admin).await;
    let patch = async |uri: &str, body: &str| send(admin, Method::PATCH, uri, body).await;

    // One key of a pool; the backend added at runtime stays a member.
    let add = format!("{{\"address\": \"{second}\"}}");
    let (status, _) = send(admin, Method::POST, "/pools/api/upstreams", &add).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, json) = patch("/config/upstream/api", r#"{"policy": "first"}"#).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["patched"], true);
    let (_, pool) = get(admin, "/pools/api").await;
    assert_eq!(pool["policy"], "first");
    assert_eq!(addresses(&pool), [first.to_string(), second.to_string()]);
    for _ in 0..4 {
        assert_eq!(body_of(web).await, "first", "`first` never rotates");
    }
    assert_eq!(fs::read_to_string(&config_path).expect("read"), file);

    // A TOML body, replacing the member list.
    let backends = format!("backends = [\"{second}\"]\n");
    let (status, json) = patch("/config/upstream/api", &backends).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    util::eventually(async || body_of(web).await == "second").await;
    let (_, config) = get(admin, "/config").await;
    assert_eq!(config["upstream"]["api"]["policy"], "first", "kept");

    // A server by name, then by index.
    let listen = format!("{{\"listen\": [\"{moved}\"]}}");
    let (status, json) = patch("/config/server/web", &listen).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    util::eventually(async || TcpStream::connect(web).await.is_err()).await;
    assert_eq!(body_of(moved).await, "second");
    let (status, json) = patch("/config/server/0", r#"{"name": "front"}"#).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (_, status) = get(admin, "/status").await;
    assert_eq!(status["servers"][0]["name"], "front");
    assert_eq!(status["servers"][0]["listen"], moved.to_string());

    // Unknown targets.
    for (uri, expected) in [
        ("/config/upstream/nope", "no pool named nope"),
        ("/config/server/web", "no server named web"),
        ("/config/server/1", "no server named 1"),
        (
            "/config/admin/listen",
            "no route for PATCH /config/admin/listen",
        ),
        ("/config/upstream", "no route for PATCH /config/upstream"),
    ] {
        let (status, json) = patch(uri, "{}").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {json}");
        assert_eq!(error_text(&json), expected);
    }

    // Rejected patches leave the running configuration alone.
    for (uri, body, expected) in [
        ("/config/upstream/api", "{oops", "invalid JSON body"),
        ("/config/upstream/api", "= 1", "invalid TOML body"),
        ("/config/upstream/api", r#"{"polcy": "random"}"#, "polcy"),
        ("/config/upstream/api", r#"{"policy": 3}"#, "invalid patch"),
        (
            "/config/server/front",
            r#"{"match": [{"forward": "@gone"}]}"#,
            "gone",
        ),
        ("/config/server/front", r#"{"listen": null}"#, "listen"),
    ] {
        let (status, json) = patch(uri, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json}");
        assert!(error_text(&json).contains(expected), "{json}");
        assert_eq!(body_of(moved).await, "second");
    }

    // A reload goes back to the file.
    let (status, _) = send(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK);
    util::eventually(async || body_of(web).await == "first").await;
    let (_, pool) = get(admin, "/pools/api").await;
    assert_eq!(pool["policy"], "weighted_round_robin");
}

#[tokio::test(flavor = "multi_thread")]
async fn dashboard_is_a_page_served_to_local_browsers_only() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(&config_path, format!("[admin]\nlisten = \"{admin}\"\n")).expect("write");
    let _daemon = start(&config_path, admin).await;

    let (status, headers, body) = util::get(admin, "/dashboard").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    let policy = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(policy.starts_with("default-src 'none';"), "{policy}");
    assert!(policy.contains("connect-src 'self'"), "{policy}");
    assert!(policy.contains("frame-ancestors 'none'"), "{policy}");
    let page = String::from_utf8(body.to_vec()).expect("UTF-8 page");
    assert!(page.starts_with("<!doctype html>"));
    for section in ["Listeners", "Hosts", "Health", "Certificates"] {
        assert!(page.contains(&format!("<h2>{section}</h2>")), "{section}");
    }

    // The page calls the API with relative URLs: from `/dashboard/` they
    // would resolve to `/dashboard/status`, so that path redirects instead.
    for uri in ["/dashboard/", "//dashboard"] {
        let (status, headers, body) = util::get(admin, uri).await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT, "{uri}");
        assert_eq!(headers[header::LOCATION], "/dashboard", "{uri}");
        assert!(body.is_empty(), "{uri}");
    }
    // Every URL the page fetches answers from where the page is served.
    for route in ["/status", "/config", "/certificates"] {
        let (status, _, _) = util::get(admin, route).await;
        assert_eq!(status, StatusCode::OK, "{route}");
    }

    // The page is read only, and so is its route.
    let (status, _) = send(admin, Method::POST, "/dashboard", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A page of another site cannot load it through DNS rebinding.
    let foreign = [(header::HOST, "evil.example")];
    let (status, _, _) = call(admin, Method::GET, "/dashboard", "", &foreign).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

const TOKEN: &str = "s3cret-admin-token-0123";

#[tokio::test(flavor = "multi_thread")]
async fn token_is_required_on_every_route_when_set() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n"),
    )
    .expect("write");
    let daemon = start(&config_path, admin).await;

    let wrong = format!("Bearer {TOKEN}x");
    let basic = format!("Basic {TOKEN}");
    for (method, uri) in [
        (Method::GET, "/status"),
        (Method::GET, "/metrics"),
        (Method::GET, "/config"),
        (Method::POST, "/reload"),
        (Method::POST, "/load"),
        (Method::PATCH, "/config/upstream/api"),
        (Method::POST, "/stop"),
    ] {
        for headers in [
            [].as_slice(),
            &[(header::AUTHORIZATION, wrong.as_str())],
            &[(header::AUTHORIZATION, basic.as_str())],
            &[(header::AUTHORIZATION, "Bearer")],
        ] {
            let (status, response, json) = call(admin, method.clone(), uri, "", headers).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} {headers:?}"
            );
            assert_eq!(response[header::WWW_AUTHENTICATE], "Bearer");
            assert_eq!(
                error_text(&json),
                "admin API requires Authorization: Bearer <token>"
            );
        }
    }

    // The dashboard page holds no data and a browser cannot send the token
    // when it opens it, so only that page is served without one.
    let (status, _, page) = util::get(admin, "/dashboard").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.starts_with(b"<!doctype html>"));
    assert!(
        !page
            .windows(TOKEN.len())
            .any(|part| part == TOKEN.as_bytes())
    );
    // Its trailing slash form redirects there without the token as well.
    let (status, headers, _) = util::get(admin, "/dashboard/").await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(headers[header::LOCATION], "/dashboard");
    for (method, uri) in [(Method::POST, "/dashboard"), (Method::GET, "/dashboard/x")] {
        let (status, _, _) = call(admin, method.clone(), uri, "", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }

    // The token replaces the loopback `Host` check: remote names are fine.
    let bearer = format!("Bearer {TOKEN}");
    let remote = [
        (header::AUTHORIZATION, bearer.as_str()),
        (header::HOST, "proxy.example.com:2019"),
    ];
    let (status, _, _) = call(admin, Method::GET, "/status", "", &remote).await;
    assert_eq!(status, StatusCode::OK);

    // The token never leaves the process.
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let (status, _, config) = call(admin, Method::GET, "/config", "", &authorized).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["admin"]["token"], "***");

    // A reload cannot swap the token of the running listener.
    fs::write(&config_path, format!("[admin]\nlisten = \"{admin}\"\n")).expect("write");
    let (status, _, _) = call(admin, Method::POST, "/reload", "", &authorized).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call(admin, Method::GET, "/status", "", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "token still required");

    let (status, _, _) = call(admin, Method::POST, "/stop", "", &authorized).await;
    assert_eq!(status, StatusCode::OK);
    daemon.await.expect("no panic").expect("clean stop");
}

#[cfg(unix)]
mod unix_socket {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixListener;

    use tokio::net::UnixStream;

    use super::*;

    async fn call_unix(
        socket: &Path,
        method: Method,
        uri: &str,
        headers: &[(header::HeaderName, &str)],
    ) -> (StatusCode, Value) {
        let stream = UnixStream::connect(socket).await.expect("socket accepts");
        let (status, _, json) = call_on(stream, "localhost", method, uri, "", headers).await;
        (status, json)
    }

    fn config(socket: &Path, rest: &str) -> Config {
        Config::from_toml(&format!(
            "[admin]\nlisten = \"unix/{}\"\n{rest}",
            socket.display()
        ))
        .expect("valid")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn admin_api_is_served_on_a_private_socket_and_removed_on_stop() {
        let directory = tempfile::tempdir().expect("temp dir");
        let socket = directory.path().join("admin.sock");
        // A dead daemon left its socket behind: it is replaced.
        drop(UnixListener::bind(&socket).expect("stale socket"));
        assert!(socket.exists());

        let daemon = tokio::task::spawn(Daemon::from_config(config(&socket, "")).run());
        util::eventually(async || UnixStream::connect(&socket).await.is_ok()).await;

        let metadata = fs::metadata(&socket).expect("socket file");
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(leftovers, ["admin.sock"], "no temporary socket left");

        let (status, json) = call_unix(&socket, Method::GET, "/status", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["admin"], format!("unix/{}", socket.display()));
        // No `Host` check: web pages cannot open unix sockets.
        let foreign = [(header::HOST, "proxy.example.com")];
        let (status, _) = call_unix(&socket, Method::GET, "/pools", &foreign).await;
        assert_eq!(status, StatusCode::OK);

        // A second daemon does not steal the socket of a live one.
        let err = Daemon::from_config(config(&socket, ""))
            .run()
            .await
            .expect_err("socket is taken")
            .to_string();
        assert!(
            err.contains("a process is listening on the socket"),
            "{err}"
        );
        assert!(err.contains("is another rxh running?"), "{err}");
        let (status, _) = call_unix(&socket, Method::GET, "/status", &[]).await;
        assert_eq!(status, StatusCode::OK, "the first daemon keeps its socket");

        let (status, json) = call_unix(&socket, Method::POST, "/stop", &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["stopping"], true);
        daemon.await.expect("no panic").expect("clean stop");
        util::eventually(async || !socket.exists()).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn token_is_checked_on_the_socket_too() {
        let directory = tempfile::tempdir().expect("temp dir");
        let socket = directory.path().join("admin.sock");
        let rest = format!("token = \"{TOKEN}\"\n");
        let _daemon = tokio::task::spawn(Daemon::from_config(config(&socket, &rest)).run());
        util::eventually(async || UnixStream::connect(&socket).await.is_ok()).await;

        let (status, _) = call_unix(&socket, Method::GET, "/status", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let bearer = format!("Bearer {TOKEN}");
        let authorized = [(header::AUTHORIZATION, bearer.as_str())];
        let (status, _) = call_unix(&socket, Method::GET, "/status", &authorized).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_that_are_not_sockets_are_never_replaced() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("precious.txt");
        fs::write(&path, "keep me").expect("write");

        let err = Daemon::from_config(config(&path, ""))
            .run()
            .await
            .expect_err("not a socket")
            .to_string();
        assert!(err.contains("the path exists and is not a socket"), "{err}");
        assert_eq!(fs::read_to_string(&path).expect("still there"), "keep me");

        let missing = directory.path().join("missing/admin.sock");
        let err = Daemon::from_config(config(&missing, ""))
            .run()
            .await
            .expect_err("no such directory")
            .to_string();
        assert!(err.contains("admin API on unix/"), "{err}");
    }
}
