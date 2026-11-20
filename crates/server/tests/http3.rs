//! HTTP/3 on `http` servers with TLS: requests through the QUIC listener of a
//! running daemon.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::{SocketAddr, UdpSocket};
use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rxh_server::Daemon;
use rxh_test_util::{Fault, FlakyBackend, Http3Client, free_address};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};

/// Names the test certificate is valid for.
const NAMES: [&str; 3] = ["localhost", "a.localhost", "b.localhost"];

/// What the `[[server]]` sections of a test need.
struct Site {
    /// Address to listen on, TCP and UDP.
    listen: SocketAddr,
    /// Value of the `tls` key: a certificate for [`NAMES`].
    tls: String,
    /// Temporary directory of the test, for other files.
    dir: PathBuf,
}

/// A daemon serving a test configuration.
struct Proxy {
    address: SocketAddr,
    /// Certificate of the servers, which the clients trust.
    root: String,
    _dir: TempDir,
}

impl Proxy {
    /// Starts a daemon with the configuration `config` returns, plus an
    /// `[admin]` table, and waits until it serves.
    async fn start(config: impl FnOnce(&Site) -> String) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let certificate =
            rcgen::generate_simple_self_signed(NAMES.map(String::from).to_vec()).unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, certificate.cert.pem()).unwrap();
        std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();
        let site = Site {
            listen: free_address(),
            tls: format!("{{ cert = {cert:?}, key = {key:?} }}"),
            dir: dir.path().to_owned(),
        };

        let admin = free_address();
        let text = format!("{}\n[admin]\nlisten = \"{admin}\"\n", config(&site));
        let path = dir.path().join("rxh.toml");
        std::fs::write(&path, text).unwrap();
        tokio::task::spawn(Daemon::new(&path).run());
        util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

        Self {
            address: site.listen,
            root: certificate.cert.pem(),
            _dir: dir,
        }
    }

    /// A new QUIC connection for `name`.
    async fn connect(&self, name: &str) -> Http3Client {
        Http3Client::connect(self.address, name, &self.root, None)
            .await
            .expect("QUIC handshake")
    }

    /// `GET https://localhost{path}` on a new connection.
    async fn get(&self, path: &str) -> rxh_test_util::Http3Response {
        let request = Request::get(format!("https://localhost{path}"))
            .body(Bytes::new())
            .unwrap();
        self.connect("localhost").await.send(request).await.unwrap()
    }
}

/// A single `http` server forwarding to `backend`, with `extra` keys.
fn forwarding(site: &Site, backend: SocketAddr, extra: &str) -> String {
    format!(
        "[[server]]\nlisten = \"{}\"\nforward = \"{backend}\"\ntls = {}\n{extra}\n",
        site.listen, site.tls
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn get_over_http3() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "intercepted");
    let seen = requests.recv().await.unwrap();
    assert_eq!(seen.headers["x-forwarded-proto"], "https");
}

#[tokio::test(flavor = "multi_thread")]
async fn get_over_http3_with_per_core_runtime() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy =
        Proxy::start(|site| format!("runtime = \"per_core\"\n{}", forwarding(site, backend, "")))
            .await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "intercepted");
    let seen = requests.recv().await.unwrap();
    assert_eq!(seen.headers["x-forwarded-proto"], "https");
}

#[tokio::test(flavor = "multi_thread")]
async fn post_body_reaches_the_backend() {
    let (backend, _) = util::echo_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let body: Bytes = (0..300_000_u32).map(|index| (index % 251) as u8).collect();

    let request = Request::post("https://localhost/upload")
        .body(body.clone())
        .unwrap();
    let response = proxy
        .connect("localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.len(), body.len());
    assert!(response.body == body, "the echoed body differs");
}

#[tokio::test(flavor = "multi_thread")]
async fn large_response_crosses_flow_control_windows() {
    let large: Bytes = (0..8 * 1024 * 1024_u32)
        .map(|index| (index % 253) as u8)
        .collect();
    let expected = Sha256::digest(&large);
    let backend = util::backend_with(move |_| {
        let large = large.clone();
        async move { Response::new(Full::new(large)) }
    })
    .await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let response = proxy.get("/large").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body.len(), 8 * 1024 * 1024);
    assert_eq!(Sha256::digest(&response.body), expected);
}

/// Response body: one data frame, then a trailer `x-checksum: abc`.
struct WithTrailer {
    data: Option<Bytes>,
    done: bool,
}

impl hyper::body::Body for WithTrailer {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        if self.done {
            return Poll::Ready(None);
        }
        self.done = true;
        let mut trailers = HeaderMap::new();
        trailers.insert("x-checksum", "abc".parse().unwrap());
        Poll::Ready(Some(Ok(Frame::trailers(trailers))))
    }
}

/// HTTP/2 cleartext backend answering `checked` and a trailer.
async fn trailer_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = service_fn(|_: Request<Incoming>| async {
                Ok::<_, Infallible>(Response::new(WithTrailer {
                    data: Some(Bytes::from("checked")),
                    done: false,
                }))
            });
            tokio::task::spawn(
                hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service),
            );
        }
    });
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn response_trailers_are_forwarded() {
    let backend = trailer_backend().await;
    let proxy = Proxy::start(|site| {
        format!(
            "[upstream.h2]\nbackends = [\"{backend}\"]\nh2c = true\n\n\
             [[server]]\nlisten = \"{}\"\nforward = \"@h2\"\ntls = {}\n",
            site.listen, site.tls
        )
    })
    .await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "checked");
    let trailers = response.trailers.expect("trailers");
    assert_eq!(trailers["x-checksum"], "abc");
}

#[tokio::test(flavor = "multi_thread")]
async fn routes_by_authority() {
    let a = util::backend("a").await;
    let b = util::backend("b").await;
    let proxy = Proxy::start(|site| {
        format!(
            "[[server]]\nlisten = \"{listen}\"\nhost = [\"a.localhost\"]\nforward = \"{a}\"\n\
             tls = {tls}\n\n\
             [[server]]\nlisten = \"{listen}\"\nhost = [\"b.localhost\"]\nforward = \"{b}\"\n\
             tls = {tls}\n",
            listen = site.listen,
            tls = site.tls
        )
    })
    .await;

    let request = Request::get("https://b.localhost/")
        .body(Bytes::new())
        .unwrap();
    let response = proxy
        .connect("b.localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn head_and_204_have_no_body() {
    let hello = util::backend("hello").await;
    let empty = util::status_backend(204).await;
    let proxy = Proxy::start(|site| {
        format!(
            "[[server]]\nlisten = \"{}\"\ntls = {}\n\
             [[server.match]]\nuri = \"/empty\"\nforward = \"{empty}\"\n\
             [[server.match]]\nforward = \"{hello}\"\n",
            site.listen, site.tls
        )
    })
    .await;
    let mut client = proxy.connect("localhost").await;

    let request = Request::builder()
        .method(Method::HEAD)
        .uri("https://localhost/")
        .body(Bytes::new())
        .unwrap();
    let head = client.send(request).await.unwrap();
    let request = Request::get("https://localhost/empty")
        .body(Bytes::new())
        .unwrap();
    let no_content = client.send(request).await.unwrap();

    assert_eq!(head.status, StatusCode::OK);
    assert_eq!(head.headers["content-length"], "5");
    assert!(head.body.is_empty(), "HEAD answered {:?}", head.body);
    assert_eq!(no_content.status, StatusCode::NO_CONTENT);
    assert!(
        no_content.body.is_empty(),
        "204 answered {:?}",
        no_content.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_fields_are_not_sent() {
    let backend = util::backend("fields").await;
    let rules = "response = { headers = { set = { connection = \"close\", \
                 keep-alive = \"timeout=5\", upgrade = \"h2c\", x-kept = \"yes\" } } }";
    let proxy = Proxy::start(|site| forwarding(site, backend, rules)).await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.headers["x-kept"], "yes");
    for name in ["connection", "keep-alive", "upgrade"] {
        assert!(!response.headers.contains_key(name), "{name} was sent");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn access_log_shows_http3() {
    let backend = util::backend("logged").await;
    let mut path = PathBuf::new();
    let proxy = Proxy::start(|site| {
        path = site.dir.join("access.log");
        format!(
            "[log]\nformat = \"json\"\noutput = {path:?}\n\n{}",
            forwarding(site, backend, "")
        )
    })
    .await;

    assert_eq!(proxy.get("/logged").await.status, StatusCode::OK);

    let mut text = String::new();
    util::eventually(async || {
        text = std::fs::read_to_string(&path).unwrap_or_default();
        !text.is_empty()
    })
    .await;
    let line: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(line["proto"], "HTTP/3.0");
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_refused_is_502() {
    let proxy = Proxy::start(|site| forwarding(site, util::dead_address(), "")).await;

    assert_eq!(proxy.get("/").await.status, StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread")]
async fn body_over_max_body_size_is_413() {
    let (backend, _) = util::echo_backend().await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "request = { max_body_size = \"1KB\" }"))
            .await;

    let request = Request::post("https://localhost/")
        .body(Bytes::from(vec![b'x'; 4000]))
        .unwrap();
    let response = proxy
        .connect("localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reset_frees_the_connection_slot() {
    let (backend, _) = util::echo_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "max_connections = 1")).await;

    let mut client = proxy.connect("localhost").await;
    let request = Request::post("https://localhost/")
        .header("content-length", "1000")
        .body(())
        .unwrap();
    client.send_and_reset(request, b"ten bytes!").await.unwrap();
    drop(client);

    let response = tokio::time::timeout(Duration::from_secs(2), proxy.get("/"))
        .await
        .expect("a second client is served within 2 s");
    assert_eq!(response.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_dying_mid_response_resets_the_stream() {
    let backend = FlakyBackend::spawn(Fault::TruncatedBody).await;
    let proxy = Proxy::start(|site| forwarding(site, backend.address(), "")).await;

    let request = Request::get("https://localhost/")
        .body(Bytes::new())
        .unwrap();
    let result = proxy.connect("localhost").await.send(request).await;

    assert!(result.is_err(), "a cut body ended normally: {result:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tls13_suites_without_aes_128_still_serve_http3() {
    let backend = util::backend("aes-256").await;
    let options = "tls_options = { cipher_suites = [\"TLS_AES_256_GCM_SHA384\"] }";
    let proxy = Proxy::start(|site| forwarding(site, backend, options)).await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "aes-256");
}

#[tokio::test(flavor = "multi_thread")]
async fn http3_false_opens_no_udp_socket() {
    let backend = util::backend("tcp only").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "http3 = false")).await;

    UdpSocket::bind(proxy.address).expect("the UDP port is free");
}
