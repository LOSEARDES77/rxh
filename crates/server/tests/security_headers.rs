//! `security_headers` presets and `cors` on servers and match entries.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rxh_test_util::free_address;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

async fn send(
    proxy: SocketAddr,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap) {
    let mut request = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let (status, headers, _) =
        util::send(proxy, request.body(Empty::<Bytes>::new()).unwrap()).await;
    (status, headers)
}

async fn get(proxy: SocketAddr, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap) {
    send(proxy, Method::GET, uri, headers).await
}

/// Backend answering with an `X-Frame-Options` and a CORS header of its own.
async fn opinionated_backend() -> SocketAddr {
    util::backend_with(|_| async {
        Response::builder()
            .header("x-frame-options", "ALLOW-FROM https://partner.test")
            .header("access-control-allow-origin", "*")
            .body(Full::from("ok"))
            .unwrap()
    })
    .await
}

#[tokio::test]
async fn presets_add_missing_headers_and_keep_the_backend_ones() {
    let backend = opinionated_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nsecurity_headers = \"basic\"\nmatch = [\n\
         {{ uri = \"/strict\", security_headers = \"strict\", forward = \"{backend}\" }},\n\
         {{ uri = \"/off\", security_headers = \"off\", forward = \"{backend}\" }},\n\
         {{ uri = \"/moved\", redirect = \"/elsewhere\" }},\n\
         {{ forward = \"{backend}\" }},\n]"
    ))[0];

    let (status, headers) = get(proxy, "/", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        headers["referrer-policy"],
        "strict-origin-when-cross-origin"
    );
    assert_eq!(
        headers["x-frame-options"], "ALLOW-FROM https://partner.test",
        "the backend's header wins"
    );
    assert_eq!(headers.get_all("x-frame-options").iter().count(), 1);
    assert!(
        !headers.contains_key("strict-transport-security"),
        "no HSTS over plain HTTP"
    );
    assert!(!headers.contains_key("content-security-policy"));

    let (_, headers) = get(proxy, "/strict", &[]).await;
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert!(
        headers["content-security-policy"]
            .to_str()
            .unwrap()
            .starts_with("default-src 'self'")
    );
    assert_eq!(headers["cross-origin-opener-policy"], "same-origin");
    assert!(headers.contains_key("permissions-policy"));

    let (_, headers) = get(proxy, "/off", &[]).await;
    assert!(!headers.contains_key("x-content-type-options"));
    assert!(!headers.contains_key("referrer-policy"));

    let (status, headers) = get(proxy, "/moved", &[]).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        headers["x-frame-options"], "SAMEORIGIN",
        "responses made by RXH get the preset too"
    );
}

#[tokio::test]
async fn hsts_is_sent_over_tls() {
    let backend = util::backend("ok").await;
    let dir = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec![String::from("a.test")]).unwrap();
    let cert = dir.path().join("a.pem");
    let key = dir.path().join("a.key");
    std::fs::write(&cert, certificate.cert.pem()).unwrap();
    std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();
    let address = free_address();
    util::spawn(&format!(
        "[[server]]\nlisten = \"{address}\"\nforward = \"{backend}\"\n\
         security_headers = \"strict\"\ntls = {{ cert = {cert:?}, key = {key:?} }}"
    ));

    let mut roots = RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let stream = TcpStream::connect(address).await.unwrap();
    let tls = TlsConnector::from(Arc::new(client))
        .connect(ServerName::try_from("a.test").unwrap(), stream)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let request = Request::get("/")
        .header("host", "a.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()["strict-transport-security"],
        "max-age=63072000; includeSubDomains"
    );
}

#[tokio::test]
async fn preflights_are_answered_without_the_backend() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         cors = {{ origins = [\"https://app.test\", \"https://*.example.test\"], \
         credentials = true, max_age = \"10m\" }}"
    ))[0];

    let preflight = |origin: &'static str| async move {
        send(
            proxy,
            Method::OPTIONS,
            "/api",
            &[
                ("origin", origin),
                ("access-control-request-method", "PUT"),
                ("access-control-request-headers", "x-token, content-type"),
            ],
        )
        .await
    };

    let (status, headers) = preflight("https://app.test").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["access-control-allow-origin"], "https://app.test");
    assert_eq!(headers["access-control-allow-credentials"], "true");
    assert_eq!(
        headers["access-control-allow-methods"],
        "GET, HEAD, POST, PUT, PATCH, DELETE"
    );
    assert_eq!(
        headers["access-control-allow-headers"], "x-token, content-type",
        "without `headers`, the requested ones are allowed"
    );
    assert_eq!(headers["access-control-max-age"], "600");
    assert_eq!(headers["vary"], "Origin");

    let (status, headers) = preflight("https://a.example.test").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers["access-control-allow-origin"],
        "https://a.example.test"
    );

    let (status, headers) = preflight("https://evil.test").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!headers.contains_key("access-control-allow-origin"));
    assert!(!headers.contains_key("access-control-allow-methods"));

    assert_eq!(counters.requests(), 0, "preflights never reach the backend");

    // A plain OPTIONS request is not a preflight.
    let (status, _) = send(proxy, Method::OPTIONS, "/api", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(counters.requests(), 1);
}

#[tokio::test]
async fn responses_carry_the_headers_of_allowed_origins_only() {
    let backend = opinionated_backend().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\n\
         cors = {{ origins = \"https://app.test\", expose_headers = [\"X-Total\", \"X-Page\"] }}\n\
         match = [\n\
         {{ uri = \"/public\", cors = {{ origins = \"*\", headers = \"X-Token\" }}, forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]"
    ))[0];

    let (status, headers) = get(proxy, "/", &[("origin", "https://app.test")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["access-control-allow-origin"], "https://app.test");
    assert_eq!(
        headers
            .get_all("access-control-allow-origin")
            .iter()
            .count(),
        1,
        "the backend's value is replaced"
    );
    assert_eq!(headers["access-control-expose-headers"], "X-Total, X-Page");
    assert!(!headers.contains_key("access-control-allow-credentials"));
    assert_eq!(headers["vary"], "Origin");

    let (_, headers) = get(proxy, "/", &[("origin", "https://evil.test")]).await;
    assert!(
        !headers.contains_key("access-control-allow-origin"),
        "the backend's `*` is dropped too"
    );
    assert_eq!(headers["vary"], "Origin");

    // The entry's `cors` replaces the server's.
    let (_, headers) = get(proxy, "/public", &[("origin", "https://evil.test")]).await;
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert!(!headers.contains_key("access-control-expose-headers"));
    assert!(!headers.contains_key("vary"), "`*` does not vary by origin");
    let (_, headers) = send(
        proxy,
        Method::OPTIONS,
        "/public",
        &[
            ("origin", "https://evil.test"),
            ("access-control-request-method", "DELETE"),
            ("access-control-request-headers", "x-other"),
        ],
    )
    .await;
    assert_eq!(headers["access-control-allow-headers"], "X-Token");
}

#[tokio::test]
async fn preflights_skip_authentication_but_requests_do_not() {
    let hash = bcrypt::hash("secret", 4).unwrap();
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         basic_auth = {{ users = {{ alice = \"{hash}\" }} }}\n\
         cors = {{ origins = \"https://app.test\", credentials = true }}"
    ))[0];

    let (status, headers) = send(
        proxy,
        Method::OPTIONS,
        "/",
        &[
            ("origin", "https://app.test"),
            ("access-control-request-method", "GET"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers["access-control-allow-origin"], "https://app.test");

    let (status, headers) = get(proxy, "/", &[("origin", "https://app.test")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["access-control-allow-origin"], "https://app.test",
        "scripts can read the 401"
    );
    assert_eq!(counters.requests(), 0);
}
