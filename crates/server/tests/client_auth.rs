//! Mutual TLS on listeners: the four `client_auth` modes and the client
//! certificate subject passed to the backends.

mod util;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::{CertificateParams, CertifiedKey, DnType, Issuer, KeyPair};
use rxh_server::Master;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Header the test servers pass the subject in.
const HEADER: &str = "x-client-subject";

/// A certificate authority for client certificates, with its certificate
/// written to a file of `dir`.
struct Authority {
    issuer: Issuer<'static, KeyPair>,
    path: String,
}

impl Authority {
    fn new(dir: &Path, name: &str) -> Self {
        let key = KeyPair::generate().expect("key pair");
        let mut params = CertificateParams::new(Vec::new()).expect("valid parameters");
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let path = dir.join(format!("{name}.pem"));
        let cert = params.self_signed(&key).expect("self-signed authority");
        std::fs::write(&path, cert.pem()).expect("write the authority");
        Self {
            issuer: Issuer::new(params, key),
            path: path.to_str().expect("UTF-8 path").to_owned(),
        }
    }

    /// Client certificate for `CN=name, O=Example`.
    fn client(&self, name: &str) -> CertifiedKey<KeyPair> {
        let signing_key = KeyPair::generate().expect("key pair");
        let mut params = CertificateParams::new(Vec::new()).expect("valid parameters");
        params.distinguished_name.push(DnType::CommonName, name);
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Example");
        let cert = params
            .signed_by(&signing_key, &self.issuer)
            .expect("signed certificate");
        CertifiedKey { cert, signing_key }
    }
}

/// A TLS server of `protocol` with `client_auth`, in front of `backend`.
struct Proxy {
    address: SocketAddr,
    roots: RootCertStore,
}

impl Proxy {
    fn config(dir: &Path, protocol: &str, backend: SocketAddr, client_auth: &str) -> String {
        let server = rcgen::generate_simple_self_signed(vec![String::from("mtls.test")])
            .expect("server certificate");
        let cert = dir.join("server.pem");
        let key = dir.join("server.key");
        std::fs::write(&cert, server.cert.pem()).expect("write cert");
        std::fs::write(&key, server.signing_key.serialize_pem()).expect("write key");
        format!(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"{protocol}\"\n\
             forward = \"{backend}\"\ntls = {{ cert = {cert:?}, key = {key:?} }}\n\
             client_auth = {client_auth}\n"
        )
    }

    fn spawn(dir: &Path, protocol: &str, backend: SocketAddr, client_auth: &str) -> Self {
        let address = util::spawn(&Self::config(dir, protocol, backend, client_auth))[0];
        let mut roots = RootCertStore::empty();
        let pem = std::fs::read(dir.join("server.pem")).expect("read cert");
        let (_, pem) = x509_parser::pem::parse_x509_pem(&pem).expect("PEM certificate");
        roots.add(pem.contents.into()).expect("valid certificate");
        Self { address, roots }
    }

    /// Handshake presenting `client`. With TLS 1.3 a rejected certificate
    /// only shows on the first read.
    async fn connect(
        &self,
        client: Option<&CertifiedKey<KeyPair>>,
    ) -> std::io::Result<TlsStream<TcpStream>> {
        let builder = ClientConfig::builder().with_root_certificates(self.roots.clone());
        let config = match client {
            Some(client) => {
                let key = PrivateKeyDer::try_from(client.signing_key.serialize_der())
                    .expect("PKCS#8 key");
                builder
                    .with_client_auth_cert(vec![client.cert.der().clone()], key)
                    .expect("key matches the certificate")
            }
            None => builder.with_no_client_auth(),
        };
        let stream = TcpStream::connect(self.address).await?;
        let name = ServerName::try_from("mtls.test").expect("valid name");
        TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await
    }

    /// Status of a request that tries to forge the subject header, or
    /// `None` if the proxy refused the client.
    async fn get(&self, client: Option<&CertifiedKey<KeyPair>>) -> Option<StatusCode> {
        let stream = self.connect(client).await.ok()?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .ok()?;
        tokio::task::spawn(connection);
        let request = Request::get("/")
            .header("host", "mtls.test")
            .header(HEADER, "CN=forged")
            .body(Empty::<Bytes>::new())
            .expect("valid request");
        Some(sender.send_request(request).await.ok()?.status())
    }
}

#[tokio::test]
async fn require_and_verify_accepts_only_trusted_certificates_and_passes_the_subject() {
    let dir = tempfile::tempdir().unwrap();
    let trusted = Authority::new(dir.path(), "trusted");
    let other = Authority::new(dir.path(), "other");
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::spawn(
        dir.path(),
        "http",
        backend,
        &format!(
            "{{ ca = {:?}, header = \"X-Client-Subject\" }}",
            trusted.path
        ),
    );

    assert_eq!(
        proxy.get(Some(&trusted.client("alice"))).await,
        Some(StatusCode::OK)
    );
    let received = requests.recv().await.unwrap();
    let subjects: Vec<_> = received.headers.get_all(HEADER).iter().collect();
    assert_eq!(subjects, ["CN=alice, O=Example"]);

    assert_eq!(proxy.get(None).await, None);
    assert_eq!(proxy.get(Some(&other.client("alice"))).await, None);
    assert!(requests.try_recv().is_err(), "nothing reached the backend");
}

#[tokio::test]
async fn verify_if_given_lets_anonymous_clients_in_without_a_subject() {
    let dir = tempfile::tempdir().unwrap();
    let trusted = Authority::new(dir.path(), "trusted");
    let other = Authority::new(dir.path(), "other");
    let (backend, mut requests) = util::interceptor().await;
    let proxy = Proxy::spawn(
        dir.path(),
        "http",
        backend,
        &format!(
            "{{ mode = \"verify_if_given\", ca = [{:?}], header = \"X-Client-Subject\" }}",
            trusted.path
        ),
    );

    assert_eq!(proxy.get(None).await, Some(StatusCode::OK));
    let received = requests.recv().await.unwrap();
    assert!(
        !received.headers.contains_key(HEADER),
        "the subject sent by the client must not reach the backend"
    );

    assert_eq!(
        proxy.get(Some(&trusted.client("bob"))).await,
        Some(StatusCode::OK)
    );
    let received = requests.recv().await.unwrap();
    assert_eq!(received.headers[HEADER], "CN=bob, O=Example");

    assert_eq!(proxy.get(Some(&other.client("bob"))).await, None);
}

#[tokio::test]
async fn require_and_request_do_not_verify_the_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let unknown = Authority::new(dir.path(), "unknown").client("mallory");
    let backend = util::backend("ok").await;

    let require = Proxy::spawn(dir.path(), "http", backend, "{ mode = \"require\" }");
    assert_eq!(require.get(Some(&unknown)).await, Some(StatusCode::OK));
    assert_eq!(require.get(None).await, None);

    let request = Proxy::spawn(dir.path(), "http", backend, "{ mode = \"request\" }");
    assert_eq!(request.get(Some(&unknown)).await, Some(StatusCode::OK));
    assert_eq!(request.get(None).await, Some(StatusCode::OK));
}

#[tokio::test]
async fn tcp_servers_check_client_certificates_too() {
    let dir = tempfile::tempdir().unwrap();
    let trusted = Authority::new(dir.path(), "trusted");
    let backend = util::tcp_echo("echo:").await;
    let client_auth = format!("{{ ca = {:?} }}", trusted.path);
    let proxy = Proxy::spawn(dir.path(), "tcp", backend, &client_auth);

    let mut reply = [0; 7];
    let mut stream = proxy.connect(Some(&trusted.client("alice"))).await.unwrap();
    stream.write_all(b"hi").await.unwrap();
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"echo:hi");

    let refused = async {
        let mut stream = proxy.connect(None).await?;
        stream.write_all(b"hi").await?;
        stream.read_exact(&mut reply).await
    };
    assert!(refused.await.is_err());
}

#[tokio::test]
async fn unreadable_ca_fails_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let backend = util::dead_address();
    for (ca, expected) in [
        ("missing.pem", "missing.pem"),
        ("server.key", "no certificate"),
    ] {
        let ca = dir.path().join(ca);
        let client_auth = format!("{{ ca = {ca:?} }}");
        let config = util::config(&Proxy::config(dir.path(), "http", backend, &client_auth));
        let Err(err) = Master::from_config(&config) else {
            panic!("{ca:?} must not be accepted as a ca");
        };
        assert!(err.to_string().contains(expected), "{err}");
    }
}
