//! HTTP/3 on `http` servers with TLS: requests through the QUIC listener of a
//! running daemon.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

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
use tokio::io::AsyncReadExt;
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
/// `size` bytes, encoded by hand: the h3 client refuses to send a field
/// section over the size the server announced, and sends the end of the
/// stream apart from the head.
fn request_headers(size: usize) -> Vec<u8> {
    // Required Insert Count and Base 0: static table references only.
    let mut block = vec![0, 0];
    // Indexed static fields: `:method GET`, `:scheme https`, `:path /`.
    block.extend([0xc0 | 17, 0xc0 | 23, 0xc0 | 1]);
    // `:authority` (static name 0) with a literal value.
    block.push(0x50);
    qpack_integer(&mut block, 0, 7, "localhost".len());
    block.extend(b"localhost");
    // Literal name and literal value.
    qpack_integer(&mut block, 0x20, 3, "x-big".len());
    block.extend(b"x-big");
    qpack_integer(&mut block, 0, 7, size);
    block.extend(std::iter::repeat_n(b'a', size));

    // Frame type HEADERS, then the length as a 4-byte QUIC varint.
    let length = u32::try_from(block.len()).unwrap();
    let mut frame = vec![0x01];
    frame.extend((0x8000_0000 | length).to_be_bytes());
    frame.extend(block);
    frame
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_request_headers_are_rejected() {
    let (backend, requests) = util::echo_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    // Over the 16 KiB HTTP/2 listeners accept too.
    let answer = client.send_raw(&request_headers(20_000)).await;

    // Either a `431` head (a HEADERS frame, type 1) or a reset stream.
    if let Ok(answer) = &answer {
        assert_eq!(
            answer.first(),
            Some(&0x01),
            "not a HEADERS frame: {answer:?}"
        );
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the backend got the request"
    );
    let request = Request::get("https://localhost/")
        .body(Bytes::new())
        .unwrap();
    let response = client.send(request).await.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

/// Backend that reads one request head, reports it, never answers, and
/// reports when the proxy closes the connection.
async fn silent_backend() -> (SocketAddr, mpsc::Receiver<&'static str>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (events, receiver) = mpsc::channel(4);
    tokio::task::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            if read == 0 {
                let _ = events.send("closed").await;
                return;
            }
            let head_before = received.windows(4).any(|window| window == b"\r\n\r\n");
            received.extend_from_slice(&buffer[..read]);
            if !head_before && received.windows(4).any(|window| window == b"\r\n\r\n") {
                let _ = events.send("head").await;
            }
        }
    });
    (address, receiver)
}

#[tokio::test(flavor = "multi_thread")]
async fn client_reset_cancels_the_backend_request() {
    let (backend, mut events) = silent_backend().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    // The head only: the request stream stays open, as an upload would.
    let request = Request::get("https://localhost/slow").body(()).unwrap();
    let open = client.open(request).await.unwrap();
    let head = tokio::time::timeout(Duration::from_secs(5), events.recv()).await;
    assert_eq!(head.unwrap(), Some("head"));

    let reset = Instant::now();
    open.reset();
    let closed = tokio::time::timeout(Duration::from_secs(2), events.recv()).await;
    assert_eq!(
        closed.expect("the backend request is cancelled within 2 s"),
        Some("closed")
    );
    assert!(reset.elapsed() < Duration::from_secs(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn get_with_content_length_zero_is_not_chunked() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    let request = Request::get("https://localhost/")
        .header("content-length", "0")
        .body(Bytes::new())
        .unwrap();
    let response = proxy
        .connect("localhost")
        .await
        .send(request)
        .await
        .unwrap();

    assert_eq!(response.status, StatusCode::OK);
    let seen = requests.recv().await.unwrap();
    assert!(
        !seen.headers.contains_key("transfer-encoding"),
        "sent upstream as {:?}",
        seen.headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn declared_body_over_max_body_size_never_reaches_the_backend() {
    let (backend, requests) = util::echo_backend().await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "request = { max_body_size = \"1KB\" }"))
            .await;
    let mut client = proxy.connect("localhost").await;

    // The head alone: the length it declares must decide.
    let request = Request::post("https://localhost/")
        .header("content-length", "4000")
        .body(())
        .unwrap();
    let mut open = client.open(request).await.unwrap();
    let status = tokio::time::timeout(Duration::from_secs(5), open.status()).await;

    assert_eq!(
        status.expect("answered without the body").unwrap(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the backend got the request"
    );
}

// Current thread: the client queues the HEADERS frame and the end of the
// stream before its connection task runs, so both leave in one packet and
// the request has ended when the proxy reads its head.
#[tokio::test]
async fn get_ending_with_its_head_is_not_chunked() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let client = proxy.connect("localhost").await;

    client.send_raw(&request_headers(1)).await.unwrap();

    let seen = requests.recv().await.unwrap();
    assert!(
        !seen.headers.contains_key("transfer-encoding"),
        "sent upstream as {:?}",
        seen.headers
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn get_declaring_no_body_is_retried_before_its_stream_ends() {
    let flaky = FlakyBackend::spawn(Fault::AcceptThenClose).await;
    let good = util::backend("good").await;
    let proxy = Proxy::start(|site| {
        format!(
            "[upstream.pool]\nbackends = [\"{}\", \"{good}\"]\npolicy = \"round_robin\"\n\
             retries = 1\n\n\
             [[server]]\nlisten = \"{}\"\nforward = \"@pool\"\ntls = {}\n",
            flaky.address(),
            site.listen,
            site.tls
        )
    })
    .await;
    let mut client = proxy.connect("localhost").await;

    // Round robin sends one of the two to the backend that closes: only a
    // request known to have no body can be replayed on the other one. The
    // streams stay open, so `content-length: 0` is all that says so.
    for _ in 0..2 {
        let request = Request::get("https://localhost/")
            .header("content-length", "0")
            .body(())
            .unwrap();
        let mut open = client.open(request).await.unwrap();
        let status = tokio::time::timeout(Duration::from_secs(5), open.status()).await;
        assert_eq!(status.unwrap().unwrap(), StatusCode::OK);
    }
    assert!(flaky.connections() >= 1, "the closing backend was tried");
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

#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_closes_the_connection() {
    let backend = util::backend("idle").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "idle_timeout = \"300ms\"")).await;
    let client = proxy.connect("localhost").await;

    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("an idle connection is closed within 2 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_without_a_request_is_closed() {
    let backend = util::backend("late").await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "read_header_timeout = \"300ms\"")).await;
    let client = proxy.connect("localhost").await;

    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("a connection without a request is closed within 2 s");
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
async fn stop_lets_a_request_in_flight_finish() {
    let (backend, mut arrivals) = slow_backend("finished", Duration::from_millis(500)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);

    let (client, response) = request.await.unwrap();
    let response = response.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "finished");
    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("the stopped daemon closes the connection within 2 s");
    let stopped = tokio::time::timeout(Duration::from_secs(2), proxy.daemon).await;
    stopped.expect("the daemon stops").unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn grace_period_cuts_long_requests() {
    let (backend, mut arrivals) = slow_backend("never", Duration::from_secs(3600)).await;
    let proxy = Proxy::start(|site| {
        format!(
            "grace_period = \"200ms\"\n{}",
            forwarding(site, backend, "")
        )
    })
    .await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move { client.send(get_request()).await });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);

    let stopped = tokio::time::timeout(Duration::from_secs(3), proxy.daemon).await;
    stopped
        .expect("the daemon stops within 3 s")
        .unwrap()
        .unwrap();
    let cut = tokio::time::timeout(Duration::from_secs(3), request).await;
    let cut = cut.expect("the request ends within 3 s").unwrap();
    assert!(cut.is_err(), "a cut request ended normally: {cut:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_keeps_the_connection_and_the_request() {
    let (a, mut arrivals) = slow_backend("a", Duration::from_millis(500)).await;
    let b = util::backend("b").await;
    let proxy = Proxy::start(|site| forwarding(site, a, "")).await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    proxy.reload(|site| forwarding(site, b, "")).await;

    let (mut client, response) = request.await.unwrap();
    let response = response.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "a");
    // The previous generation drains its connections: GOAWAY, then close.
    let refused = client.send(get_request()).await;
    assert!(refused.is_err(), "a draining connection took a request");
    assert_eq!(proxy.get("/").await.body, "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_turning_http3_off_closes_the_socket() {
    let backend = util::backend("off").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    assert!(
        UdpSocket::bind(proxy.address).is_err(),
        "HTTP/3 holds the UDP port"
    );

    proxy
        .reload(|site| forwarding(site, backend, "http3 = false"))
        .await;

    let freed = util::eventually(async || UdpSocket::bind(proxy.address).is_ok());
    tokio::time::timeout(Duration::from_secs(2), freed)
        .await
        .expect("the UDP port is free within 2 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_to_a_udp_server_on_the_port_works() {
    let (backend, mut arrivals) = slow_backend("never", Duration::from_secs(3600)).await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    // Without a grace period, a reload that has to stop the running
    // configuration before it starts the new one waits for this request.
    let tcp = https_get(&proxy).await;
    arrival(&mut arrivals).await;

    let reload = proxy.reload(|site| {
        format!(
            "[[server]]\nlisten = \"{}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n",
            site.listen
        )
    });
    tokio::time::timeout(Duration::from_secs(2), reload)
        .await
        .expect("the reload completes within 2 s");

    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(b"ping", proxy.address).await.unwrap();
    let mut buffer = [0; 64];
    let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("an answer within 2 s")
        .unwrap();
    assert_eq!(&buffer[..read], b"e:ping");
    tcp.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn client_without_stream_credit_is_closed() {
    let backend = util::backend("unused").await;
    let timed =
        Proxy::start(|site| forwarding(site, backend, "read_header_timeout = \"300ms\"")).await;
    let silent = timed.connect_silent().await;
    tokio::time::timeout(Duration::from_secs(2), silent.closed())
        .await
        .expect("a connection that grants no stream is closed within 2 s");

    // With the default deadline, stopping does not wait for it either.
    let stopped = Proxy::start(|site| {
        format!(
            "grace_period = \"200ms\"\n{}",
            forwarding(site, backend, "")
        )
    })
    .await;
    let _silent = stopped.connect_silent().await;
    assert_eq!(stopped.admin_post("/stop").await, StatusCode::OK);
    let done = tokio::time::timeout(Duration::from_secs(3), stopped.daemon).await;
    done.expect("the daemon stops within 3 s").unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_reload_keeps_http3() {
    let backend = util::backend("kept").await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    // The UDP port goes to a `udp` server, but the configuration cannot
    // start: the certificate file of the other server does not exist.
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
    let mut client = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect())
        .await
        .expect("a handshake within 2 s")
        .expect("HTTP/3 still served");
    let response = client.send(get_request()).await.unwrap();
    assert_eq!(response.body, "kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn unvalidated_clients_get_a_retry_when_slots_are_scarce() {
    let backend = util::backend("retried").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "max_connections = 3")).await;
    let _first = proxy.connect("localhost").await;
    let _second = proxy.connect("localhost").await;

    // One slot of three left: the server asks for a Retry first, which
    // the client answers on its own. That a Retry happened is not visible
    // from the client: the unit test of `needs_retry` covers the decision.
    let mut third = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect())
        .await
        .expect("a handshake within 2 s")
        .expect("the last slot is served");

    let response = third.send(get_request()).await.unwrap();
    assert_eq!(response.body, "retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_off_then_on_reuses_the_socket() {
    let (slow, mut arrivals) = slow_backend("slow", Duration::from_millis(1500)).await;
    let fast = util::backend("fast").await;
    let proxy = Proxy::start(|site| forwarding(site, slow, "")).await;
    let mut client = proxy.connect("localhost").await;
    let request = tokio::task::spawn(async move { client.send(get_request()).await });
    arrival(&mut arrivals).await;

    proxy
        .reload(|site| forwarding(site, fast, "http3 = false"))
        .await;
    proxy.reload(|site| forwarding(site, fast, "")).await;

    // Two sockets on the port would split new clients between them, and
    // the old one takes no new connection: several clients find it.
    for _ in 0..10 {
        let client = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect()).await;
        let mut client = client
            .expect("a handshake within 2 s")
            .expect("a new client is served");
        assert_eq!(client.send(get_request()).await.unwrap().body, "fast");
    }
    let response = request.await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "slow");
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
async fn drain_does_not_wait_forever_for_a_pinging_client() {
    let (backend, mut arrivals) = slow_backend("pinged", Duration::from_millis(300)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    // Pings more often than the server's quiet window: the server keeps
    // acknowledging them.
    let mut client = Http3Client::connect_keep_alive(
        proxy.address,
        "localhost",
        &proxy.root,
        Duration::from_millis(20),
    )
    .await
    .expect("QUIC handshake");

    // The stop comes while the request is in flight, so the connection
    // waits for its response to be acknowledged before it closes.
    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);
    let (client, response) = request.await.unwrap();
    assert_eq!(response.unwrap().status, StatusCode::OK);

    let stopped = tokio::time::timeout(Duration::from_secs(6), proxy.daemon).await;
    stopped
        .expect("the daemon stops within 6 s")
        .unwrap()
        .unwrap();
    drop(client);
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
