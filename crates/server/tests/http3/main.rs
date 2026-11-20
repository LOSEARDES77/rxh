//! HTTP/3 on `http` servers with TLS: requests through the QUIC listener of a
//! running daemon.

#![allow(clippy::unwrap_used)]

mod lifecycle;
mod messages;
#[path = "../util/mod.rs"]
mod util;

use std::convert::Infallible;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::body::{Frame, Incoming};
use hyper::header::HOST;
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::{CertificateParams, DnType, KeyPair};
use rxh_server::Daemon;
use rxh_test_util::{Fault, FlakyBackend, Http3Client, SilentConnection, free_address};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

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

/// A [`Site`] on `listen` with a new certificate in `dir`, and that
/// certificate as PEM.
fn site_in(dir: &Path, listen: SocketAddr) -> (Site, String) {
    let certificate = rcgen::generate_simple_self_signed(NAMES.map(String::from).to_vec()).unwrap();
    let cert = dir.join("cert.pem");
    let key = dir.join("key.pem");
    std::fs::write(&cert, certificate.cert.pem()).unwrap();
    std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();
    let site = Site {
        listen,
        tls: format!("{{ cert = {cert:?}, key = {key:?} }}"),
        dir: dir.to_owned(),
    };
    (site, certificate.cert.pem())
}

/// `GET /` over HTTPS on TCP to `address`, with HTTP/2 if `h2` and
/// HTTP/1.1 otherwise. Returns the status and the header fields.
async fn https_exchange(address: SocketAddr, root: &str, h2: bool) -> (StatusCode, HeaderMap) {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(root.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let mut tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![if h2 {
        b"h2".to_vec()
    } else {
        b"http/1.1".to_vec()
    }];
    let stream = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(name, stream)
        .await
        .unwrap();
    let request = Request::get("https://localhost/")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = if h2 {
        let (mut sender, connection) =
            hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
                .await
                .unwrap();
        tokio::task::spawn(connection);
        sender.send_request(request).await.unwrap()
    } else {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::task::spawn(connection);
        let request = Request::get("/")
            .header(HOST, "localhost")
            .body(Empty::<Bytes>::new())
            .unwrap();
        sender.send_request(request).await.unwrap()
    };
    (response.status(), response.headers().clone())
}

/// A daemon serving a test configuration.
struct Proxy {
    address: SocketAddr,
    /// Certificate of the servers, which the clients trust.
    root: String,
    site: Site,
    admin: SocketAddr,
    path: PathBuf,
    daemon: JoinHandle<Result<(), rxh_server::Error>>,
    _dir: TempDir,
}

impl Proxy {
    /// Starts a daemon with the configuration `config` returns, plus an
    /// `[admin]` table, and waits until it serves.
    async fn start(config: impl FnOnce(&Site) -> String) -> Self {
        Self::start_on(free_address(), config).await
    }

    /// [`Proxy::start`] with the servers on `listen`.
    async fn start_on(listen: SocketAddr, config: impl FnOnce(&Site) -> String) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let (site, root) = site_in(dir.path(), listen);

        let admin = free_address();
        let path = dir.path().join("rxh.toml");
        write_config(&path, admin, &config(&site));
        let daemon = tokio::task::spawn(Daemon::new(&path).run());
        util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

        Self {
            address: site.listen,
            root,
            site,
            admin,
            path,
            daemon,
            _dir: dir,
        }
    }

    /// Status of `POST {uri}` on the admin API.
    async fn admin_post(&self, uri: &str) -> StatusCode {
        let request = Request::post(uri)
            .header(HOST, self.admin.to_string())
            .body(Empty::new())
            .unwrap();
        util::send(self.admin, request).await.0
    }

    /// Rewrites the configuration file with what `config` returns and
    /// reloads it, which must succeed.
    async fn reload(&self, config: impl FnOnce(&Site) -> String) {
        assert_eq!(self.try_reload(config).await, StatusCode::OK);
    }

    /// Rewrites the configuration file with what `config` returns and
    /// returns the status of the reload.
    async fn try_reload(&self, config: impl FnOnce(&Site) -> String) -> StatusCode {
        write_config(&self.path, self.admin, &config(&self.site));
        self.admin_post("/reload").await
    }

    /// A new QUIC connection for `name`.
    async fn connect(&self, name: &str) -> Http3Client {
        Http3Client::connect(self.address, name, &self.root, None)
            .await
            .expect("QUIC handshake")
    }

    /// A new QUIC connection for `localhost`, or the handshake error.
    async fn try_connect(&self) -> std::io::Result<Http3Client> {
        Http3Client::connect(self.address, "localhost", &self.root, None).await
    }

    /// A QUIC connection for `localhost` that sends nothing and grants no
    /// stream to the server.
    async fn connect_silent(&self) -> SilentConnection {
        Http3Client::connect_silent(self.address, "localhost", &self.root)
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

/// `GET https://localhost/`.
fn get_request() -> Request<Bytes> {
    Request::get("https://localhost/")
        .body(Bytes::new())
        .unwrap()
}

/// Writes `config` and an `[admin]` table on `admin` to `path`.
fn write_config(path: &Path, admin: SocketAddr, config: &str) {
    let text = format!("{config}\n[admin]\nlisten = \"{admin}\"\n");
    std::fs::write(path, text).unwrap();
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
    assert!(
        proxy.try_connect().await.is_err(),
        "the first client holds the only slot"
    );
    drop(client);

    let mut second = None;
    let freed = util::eventually(async || {
        second = proxy.try_connect().await.ok();
        second.is_some()
    });
    tokio::time::timeout(Duration::from_secs(2), freed)
        .await
        .expect("a second client gets the slot within 2 s");
    let response = second.unwrap().send(get_request()).await.unwrap();
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

/// Appends `value` as a QPACK prefixed integer (RFC 9204, section 4.1.1):
/// `flags` in the high bits of the first byte, `prefix` bits for the value.
fn qpack_integer(out: &mut Vec<u8>, flags: u8, prefix: u32, value: usize) {
    let max = (1 << prefix) - 1;
    if value < max {
        out.push(flags | value as u8);
        return;
    }
    out.push(flags | max as u8);
    let mut rest = value - max;
    while rest >= 0x80 {
        out.push(0x80 | (rest & 0x7f) as u8);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// HEADERS frame of `GET https://localhost/` with an `x-big` field of
/// `size` bytes: the h3 client refuses to send a field section over the
/// size the server announced, and sends the end of the stream apart from
/// the head.
fn request_headers(size: usize) -> Vec<u8> {
    headers_frame("GET", "/", &[("x-big", &"a".repeat(size))])
}

/// HEADERS frame of a `method` (`GET` or `POST`) request for
/// `https://localhost{path}` with `fields`, encoded by hand, for requests
/// the h3 client would not send as they are.
fn headers_frame(method: &str, path: &str, fields: &[(&str, &str)]) -> Vec<u8> {
    let method = match method {
        "GET" => 17,
        "POST" => 20,
        _ => panic!("no static table entry for {method}"),
    };
    // Required Insert Count and Base 0: static table references only.
    let mut block = vec![0, 0];
    // Indexed static fields: the method and `:scheme https`.
    block.extend([0xc0 | method, 0xc0 | 23]);
    // `:path` (static name 1) and `:authority` (static name 0) with literal
    // values.
    for (index, value) in [(1, path), (0, "localhost")] {
        block.push(0x50 | index);
        qpack_integer(&mut block, 0, 7, value.len());
        block.extend(value.as_bytes());
    }
    // Literal names and literal values.
    for (name, value) in fields {
        qpack_integer(&mut block, 0x20, 3, name.len());
        block.extend(name.as_bytes());
        qpack_integer(&mut block, 0, 7, value.len());
        block.extend(value.as_bytes());
    }
    frame(0x01, &block)
}

/// DATA frame carrying `data`.
fn data_frame(data: &[u8]) -> Vec<u8> {
    frame(0x00, data)
}

/// HTTP/3 frame of type `kind`, its length as a 4-byte QUIC varint.
fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap();
    let mut frame = vec![kind];
    frame.extend((0x8000_0000 | length).to_be_bytes());
    frame.extend(payload);
    frame
}

/// Backend answering `body` `delay` after each request, and reporting each
/// request when it arrives.
async fn slow_backend(body: &'static str, delay: Duration) -> (SocketAddr, mpsc::Receiver<()>) {
    let (arrived, arrivals) = mpsc::channel(8);
    let address = util::backend_with(move |_| {
        let arrived = arrived.clone();
        async move {
            let _ = arrived.send(()).await;
            tokio::time::sleep(delay).await;
            Response::new(Full::new(Bytes::from(body)))
        }
    })
    .await;
    (address, arrivals)
}

/// Waits for the next request `arrivals` reports.
async fn arrival(arrivals: &mut mpsc::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), arrivals.recv())
        .await
        .expect("the backend gets the request within 5 s")
        .expect("the backend runs");
}

/// Sends `GET /` over HTTPS on TCP to `proxy` and returns the task that
/// waits for the answer.
async fn https_get(proxy: &Proxy) -> JoinHandle<hyper::Result<Response<Incoming>>> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(proxy.root.as_bytes()) {
        roots.add(cert.unwrap()).unwrap();
    }
    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let stream = TcpStream::connect(proxy.address).await.unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let stream = TlsConnector::from(Arc::new(tls))
        .connect(name, stream)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let request = Request::get("/")
        .header(HOST, "localhost")
        .body(Empty::<Bytes>::new())
        .unwrap();
    tokio::task::spawn(async move { sender.send_request(request).await })
}

#[tokio::test(flavor = "multi_thread")]
async fn max_connections_is_shared_with_tcp() {
    let (backend, mut arrivals) = slow_backend("slow", Duration::from_secs(3600)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "max_connections = 1")).await;

    let tcp = https_get(&proxy).await;
    arrival(&mut arrivals).await;

    let refused = proxy.try_connect().await;
    assert!(refused.is_err(), "a QUIC connection over max_connections");
    tcp.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn max_connections_per_ip_counts_quic() {
    let backend = util::backend("per ip").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "max_connections_per_ip = 1")).await;

    let first = proxy.connect("localhost").await;
    assert!(
        proxy.try_connect().await.is_err(),
        "a second connection from the same address"
    );
    drop(first);

    let mut second = None;
    util::eventually(async || {
        second = proxy.try_connect().await.ok();
        second.is_some()
    })
    .await;
    let response = second.unwrap().send(get_request()).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
}

// A guard: `ip_deny` is checked per request, as for HTTP/1 and HTTP/2.
#[tokio::test(flavor = "multi_thread")]
async fn ip_deny_answers_403() {
    let backend = util::backend("denied").await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "ip_deny = [\"127.0.0.1/32\"]")).await;

    assert_eq!(proxy.get("/").await.status, StatusCode::FORBIDDEN);
}

/// Writes a certificate authority to `dir` and returns its path, with the
/// chain and key PEM of a client certificate it signed for
/// `CN=alice, O=Example`.
fn client_certificate(dir: &Path) -> (PathBuf, String, String) {
    let ca_key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params
        .distinguished_name
        .push(DnType::CommonName, "clients");
    let ca = params.self_signed(&ca_key).unwrap();
    let path = dir.join("clients.pem");
    std::fs::write(&path, ca.pem()).unwrap();
    let issuer = rcgen::Issuer::new(params, ca_key);

    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, "alice");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "Example");
    let cert = params.signed_by(&key, &issuer).unwrap();
    (path, cert.pem(), key.serialize_pem())
}

#[tokio::test(flavor = "multi_thread")]
async fn mandatory_client_auth_refuses_clients_without_a_certificate() {
    let (backend, requests) = util::echo_backend().await;
    let proxy = Proxy::start(|site| {
        let (ca, _, _) = client_certificate(&site.dir);
        forwarding(site, backend, &format!("client_auth = {{ ca = {ca:?} }}"))
    })
    .await;

    let refused = match proxy.try_connect().await {
        Ok(mut client) => client.send(get_request()).await.err(),
        Err(err) => Some(err),
    };

    assert!(
        refused.is_some(),
        "a client without a certificate was served"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_subject_reaches_the_backend() {
    let (backend, mut requests) = util::interceptor().await;
    let mut identity = None;
    let proxy = Proxy::start(|site| {
        let (ca, chain, key) = client_certificate(&site.dir);
        identity = Some((chain, key));
        let client_auth = format!("client_auth = {{ ca = {ca:?}, header = \"X-Client-Subject\" }}");
        forwarding(site, backend, &client_auth)
    })
    .await;
    let (chain, key) = identity.unwrap();

    let mut client = Http3Client::connect(
        proxy.address,
        "localhost",
        &proxy.root,
        Some((&chain, &key)),
    )
    .await
    .unwrap();
    let response = client
        .send(
            Request::get("https://localhost/")
                .header("x-client-subject", "CN=forged")
                .body(Bytes::new())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    let seen = requests.recv().await.unwrap();
    let subjects: Vec<_> = seen.headers.get_all("x-client-subject").iter().collect();
    assert_eq!(subjects, ["CN=alice, O=Example"]);
}

// A guard: the QUIC TLS configuration shares the certificate resolver of
// the TCP listener.
#[tokio::test(flavor = "multi_thread")]
async fn certificate_reload_reaches_quic() {
    let backend = util::backend("renewed").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let renewed = rcgen::generate_simple_self_signed(NAMES.map(String::from).to_vec()).unwrap();
    std::fs::write(proxy.site.dir.join("cert.pem"), renewed.cert.pem()).unwrap();
    std::fs::write(
        proxy.site.dir.join("key.pem"),
        renewed.signing_key.serialize_pem(),
    )
    .unwrap();
    assert_eq!(
        proxy.admin_post("/certificates/reload").await,
        StatusCode::OK
    );

    let root = renewed.cert.pem();
    let mut client = Http3Client::connect(proxy.address, "localhost", &root, None)
        .await
        .expect("the renewed certificate is served over QUIC");
    let response = client.send(get_request()).await.unwrap();
    assert_eq!(response.body, "renewed");
}

// A guard: the UDP socket of an IPv6 listener is IPv6 only.
#[tokio::test(flavor = "multi_thread")]
async fn ipv6_listener() {
    let backend = util::backend("six").await;
    // Kept bound without listening, so no other test takes the port.
    let socket = TcpSocket::new_v6().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind("[::1]:0".parse().unwrap()).unwrap();
    let listen = socket.local_addr().unwrap();
    let _reserved = socket;
    let proxy = Proxy::start_on(listen, |site| forwarding(site, backend, "")).await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "six");
}

#[tokio::test(flavor = "multi_thread")]
async fn alt_svc_is_advertised_over_http1_and_http2() {
    let backend = util::backend("advertised").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let expected = format!("h3=\":{}\"; ma=86400", proxy.address.port());

    for h2 in [false, true] {
        let (status, headers) = https_exchange(proxy.address, &proxy.root, h2).await;
        assert_eq!(status, StatusCode::OK);
        let values: Vec<_> = headers.get_all("alt-svc").iter().collect();
        assert_eq!(values, [expected.as_str()], "h2 = {h2}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn alt_svc_is_absent_over_http3() {
    let backend = util::backend("quiet").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let response = proxy.get("/").await;

    assert_eq!(response.status, StatusCode::OK);
    assert!(!response.headers.contains_key("alt-svc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn alt_svc_is_absent_with_http3_false() {
    let backend = util::backend("tcp only").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "http3 = false")).await;

    let (status, headers) = https_exchange(proxy.address, &proxy.root, false).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("alt-svc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn backend_alt_svc_is_kept() {
    let backend = util::backend_with(|_| async {
        Response::builder()
            .header("alt-svc", "clear")
            .body(Full::new(Bytes::from("own")))
            .unwrap()
    })
    .await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    for h2 in [false, true] {
        let (_, headers) = https_exchange(proxy.address, &proxy.root, h2).await;
        let values: Vec<_> = headers.get_all("alt-svc").iter().collect();
        assert_eq!(values, ["clear"], "h2 = {h2}");
    }
}

/// Starts a daemon whose UDP port is held by `taken`, with `extra` keys on
/// its one HTTPS server. Returns what a TCP client saw, if the daemon
/// started, and the daemon's result.
async fn start_with_udp_port_taken(
    extra: &str,
) -> (
    Option<(StatusCode, HeaderMap)>,
    Result<(), rxh_server::Error>,
) {
    let dir = tempfile::tempdir().unwrap();
    let listen = free_address();
    let (site, root) = site_in(dir.path(), listen);
    let _taken = UdpSocket::bind(listen).unwrap();
    let backend = util::backend("up").await;
    let admin = free_address();
    let path = dir.path().join("rxh.toml");
    write_config(&path, admin, &forwarding(&site, backend, extra));
    let mut daemon = tokio::task::spawn(Daemon::new(&path).run());

    let started = tokio::time::timeout(Duration::from_secs(5), async {
        util::eventually(async || daemon.is_finished() || TcpStream::connect(admin).await.is_ok())
            .await
    })
    .await;
    started.expect("the daemon starts or fails within 5 s");
    if daemon.is_finished() {
        return (None, daemon.await.unwrap());
    }
    let seen = https_exchange(listen, &root, false).await;
    let stop = Request::post("/stop")
        .header(HOST, admin.to_string())
        .body(Empty::new())
        .unwrap();
    assert_eq!(util::send(admin, stop).await.0, StatusCode::OK);
    let stopped = tokio::time::timeout(Duration::from_secs(5), &mut daemon).await;
    (Some(seen), stopped.expect("the daemon stops").unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_port_taken_fails_the_start_with_http3_true() {
    let (seen, result) = start_with_udp_port_taken("http3 = true").await;

    assert!(seen.is_none(), "the daemon started");
    let err = result.expect_err("the start fails");
    assert!(err.to_string().contains("HTTP/3"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_port_taken_only_warns_when_unset() {
    let (seen, result) = start_with_udp_port_taken("").await;

    result.expect("the daemon runs and stops");
    let (status, headers) = seen.expect("the daemon started");
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("alt-svc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn alt_svc_follows_reloads() {
    let backend = util::backend("reloaded").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let advertised = async || {
        let (_, headers) = https_exchange(proxy.address, &proxy.root, false).await;
        headers.contains_key("alt-svc")
    };
    assert!(advertised().await, "advertised at the start");

    // The new generation takes over the endpoint of the old one.
    proxy.reload(|site| forwarding(site, backend, "")).await;
    assert!(advertised().await, "not advertised after a plain reload");

    proxy
        .reload(|site| forwarding(site, backend, "http3 = false"))
        .await;
    assert!(!advertised().await, "advertised with http3 = false");

    proxy.reload(|site| forwarding(site, backend, "")).await;
    assert!(advertised().await, "not advertised once HTTP/3 is back");
}

#[tokio::test(flavor = "multi_thread")]
async fn alt_svc_stays_after_a_failed_reload() {
    let backend = util::backend("kept").await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    // As in `failed_reload_keeps_http3`: the UDP port is released, then
    // the start fails.
    let status = proxy
        .try_reload(|site| {
            let missing = site.dir.join("missing.pem");
            format!(
                "[[server]]\nlisten = \"{}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n\n\
                 [[server]]\nlisten = \"{}\"\nforward = \"{backend}\"\n\
                 tls = {{ cert = {missing:?}, key = {missing:?} }}\n",
                site.listen,
                free_address()
            )
        })
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, headers) = https_exchange(proxy.address, &proxy.root, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key("alt-svc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn wildcard_ipv6_listener_serves_ipv4_clients() {
    let v6_only = std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only")
        .is_ok_and(|value| value.trim() == "1");
    if v6_only {
        eprintln!("skipped: net.ipv6.bindv6only = 1, [::] does not take IPv4");
        return;
    }
    // Kept bound without listening, so no other test takes the port.
    let reserved = TcpSocket::new_v6().and_then(|socket| {
        socket.set_reuseaddr(true)?;
        socket.bind("[::]:0".parse().unwrap())?;
        Ok(socket)
    });
    let reserved = match reserved {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("skipped: no IPv6 ({err})");
            return;
        }
    };
    let listen = reserved.local_addr().unwrap();
    let backend = util::backend("dual stack").await;
    let proxy = Proxy::start_on(listen, |site| forwarding(site, backend, "")).await;

    let ipv4 = SocketAddr::from(([127, 0, 0, 1], listen.port()));
    let connect = Http3Client::connect(ipv4, "localhost", &proxy.root, None);
    let mut client = tokio::time::timeout(Duration::from_secs(5), connect)
        .await
        .expect("a handshake within 5 s")
        .expect("QUIC handshake over IPv4");
    let response = client.send(get_request()).await.unwrap();

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "dual stack");
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_server_on_the_port_turns_http3_off() {
    let backend = util::backend("tcp").await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| {
        format!(
            "{}\n[[server]]\nlisten = \"{}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n",
            forwarding(site, backend, ""),
            site.listen
        )
    })
    .await;

    let (status, headers) = https_exchange(proxy.address, &proxy.root, false).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("alt-svc"), "HTTP/3 advertised");
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(b"ping", proxy.address).await.unwrap();
    let mut buffer = [0; 64];
    let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("an answer within 2 s")
        .unwrap();
    assert_eq!(&buffer[..read], b"e:ping");
}
