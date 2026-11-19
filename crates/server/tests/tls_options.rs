//! `tls_options` on TLS servers: minimum version, cipher suites, curves and
//! OCSP stapling.

mod util;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode};
use rxh_server::Master;
use rxh_test_util::{OcspCertificate, ocsp_response};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::WebPkiServerVerifier;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::crypto::aws_lc_rs::{self, cipher_suite, kx_group};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    self, ClientConfig, DigitallySignedStruct, ProtocolVersion, RootCertStore, SignatureScheme,
    SupportedProtocolVersion,
};

const SERIAL: [u8; 3] = OcspCertificate::SERIAL;

/// A server certificate signed by a test authority, in files of `dir`.
struct Certificate {
    dir: TempDir,
    roots: Arc<RootCertStore>,
}

impl Certificate {
    /// Certificate for `tls.test` that names `ocsp` as its OCSP responder.
    fn new(ocsp: Option<SocketAddr>) -> Self {
        let dir = tempfile::tempdir().expect("temporary directory");
        let url = ocsp.map(|ocsp| format!("http://{ocsp}/ocsp"));
        let certificate = OcspCertificate::new(url.as_deref());
        std::fs::write(dir.path().join("chain.pem"), certificate.chain_pem).expect("write chain");
        std::fs::write(dir.path().join("key.pem"), certificate.key_pem).expect("write key");
        let mut roots = RootCertStore::empty();
        roots
            .add(certificate.authority_der.into())
            .expect("valid authority");
        Self {
            dir,
            roots: Arc::new(roots),
        }
    }

    fn config(&self, tls_options: &str) -> String {
        let cert = self.dir.path().join("chain.pem");
        let key = self.dir.path().join("key.pem");
        // The handshake is all these tests look at: the backend is dead.
        let backend = util::dead_address();
        format!(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = \"{backend}\"\n\
             tls = {{ cert = {cert:?}, key = {key:?} }}\n{tls_options}\n"
        )
    }

    /// Starts a TLS server with this certificate and `tls_options`.
    fn serve(&self, tls_options: &str) -> Server {
        Server {
            address: util::spawn(&self.config(tls_options))[0],
            roots: Arc::clone(&self.roots),
        }
    }
}

struct Server {
    address: SocketAddr,
    roots: Arc<RootCertStore>,
}

impl Server {
    /// Handshake of a client limited to `provider` and `versions`. Returns
    /// the connection and the OCSP response the server stapled, empty if
    /// none.
    async fn connect(
        &self,
        provider: CryptoProvider,
        versions: &[&'static SupportedProtocolVersion],
    ) -> std::io::Result<(TlsStream<TcpStream>, Vec<u8>)> {
        let provider = Arc::new(provider);
        let inner =
            WebPkiServerVerifier::builder_with_provider(Arc::clone(&self.roots), provider.clone())
                .build()
                .expect("valid roots");
        let verifier = Arc::new(StapleRecorder {
            inner,
            staple: Mutex::default(),
        });
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("usable versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::clone(&verifier) as Arc<dyn ServerCertVerifier>)
            .with_no_client_auth();
        let stream = TcpStream::connect(self.address).await?;
        let name = ServerName::try_from("tls.test").expect("valid name");
        let stream = TlsConnector::from(Arc::new(config))
            .connect(name, stream)
            .await?;
        let staple = verifier
            .staple
            .lock()
            .expect("no test panics holding the lock")
            .clone();
        Ok((stream, staple))
    }

    /// OCSP response stapled to a handshake, empty if none.
    async fn staple(&self) -> Vec<u8> {
        let (_, staple) = self
            .connect(aws_lc_rs::default_provider(), rustls::DEFAULT_VERSIONS)
            .await
            .expect("handshake");
        staple
    }
}

/// Verifies the server certificate like any client and keeps the OCSP
/// response stapled to it.
#[derive(Debug)]
struct StapleRecorder {
    inner: Arc<WebPkiServerVerifier>,
    staple: Mutex<Vec<u8>>,
}

impl ServerCertVerifier for StapleRecorder {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        *self.staple.lock().expect("no test panics holding the lock") = ocsp_response.to_vec();
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

const TLS12: &[&SupportedProtocolVersion] = &[&rustls::version::TLS12];

#[tokio::test]
async fn min_version_refuses_older_clients() {
    let certificate = Certificate::new(None);
    let default = aws_lc_rs::default_provider;

    let server = certificate.serve("");
    let (stream, _) = server.connect(default(), TLS12).await.unwrap();
    let version = stream.get_ref().1.protocol_version();
    assert_eq!(version, Some(ProtocolVersion::TLSv1_2));

    let server = certificate.serve("tls_options = { min_version = \"1.3\" }");
    assert!(server.connect(default(), TLS12).await.is_err());
    let (stream, _) = server
        .connect(default(), rustls::DEFAULT_VERSIONS)
        .await
        .unwrap();
    let version = stream.get_ref().1.protocol_version();
    assert_eq!(version, Some(ProtocolVersion::TLSv1_3));
}

#[tokio::test]
async fn cipher_suites_limit_what_is_negotiated() {
    let certificate = Certificate::new(None);
    let server = certificate.serve(
        "tls_options = { cipher_suites = [\"TLS_AES_256_GCM_SHA384\", \
         \"TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256\"] }",
    );
    let default = aws_lc_rs::default_provider;
    let suite = |stream: &TlsStream<TcpStream>| {
        let suite = stream.get_ref().1.negotiated_cipher_suite().unwrap();
        suite.suite()
    };

    // The suites a default client prefers are off: it gets the ones listed.
    let (stream, _) = server
        .connect(default(), rustls::DEFAULT_VERSIONS)
        .await
        .unwrap();
    assert_eq!(
        suite(&stream),
        cipher_suite::TLS13_AES_256_GCM_SHA384.suite()
    );
    let (stream, _) = server.connect(default(), TLS12).await.unwrap();
    assert_eq!(
        suite(&stream),
        cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256.suite()
    );

    for unlisted in [
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ] {
        let provider = CryptoProvider {
            cipher_suites: vec![unlisted],
            ..default()
        };
        let refused = server.connect(provider, rustls::DEFAULT_VERSIONS).await;
        assert!(refused.is_err(), "{unlisted:?}");
    }
}

#[tokio::test]
async fn curves_limit_the_key_exchange() {
    let certificate = Certificate::new(None);
    let server = certificate.serve("tls_options = { curves = [\"secp384r1\"] }");
    let default = aws_lc_rs::default_provider;

    // A default client prefers X25519 and has to fall back.
    let (stream, _) = server
        .connect(default(), rustls::DEFAULT_VERSIONS)
        .await
        .unwrap();
    let group = stream.get_ref().1.negotiated_key_exchange_group().unwrap();
    assert_eq!(group.name(), kx_group::SECP384R1.name());

    let provider = CryptoProvider {
        kx_groups: vec![kx_group::X25519],
        ..default()
    };
    let refused = server.connect(provider, rustls::DEFAULT_VERSIONS).await;
    assert!(refused.is_err());
}

/// What an OCSP request looked like: its content type and body.
type Requests = Arc<Mutex<Vec<(String, Bytes)>>>;

/// An OCSP responder answering with `status` and `body`.
async fn responder(status: StatusCode, body: Vec<u8>) -> (SocketAddr, Requests) {
    let requests = Requests::default();
    let seen = Arc::clone(&requests);
    let address = util::backend_with(move |request| {
        let seen = Arc::clone(&seen);
        let body = body.clone();
        async move {
            let content_type = request.headers()["content-type"]
                .to_str()
                .expect("an ASCII content type")
                .to_owned();
            let method = request.method().clone();
            let path = request.uri().path().to_owned();
            let received = request
                .into_body()
                .collect()
                .await
                .expect("a complete request body")
                .to_bytes();
            assert_eq!((method.as_str(), path.as_str()), ("POST", "/ocsp"));
            seen.lock()
                .expect("no test panics holding the lock")
                .push((content_type, received));
            let mut response = Response::new(Full::from(body));
            *response.status_mut() = status;
            response
        }
    })
    .await;
    (address, requests)
}

/// Waits for the responder to be asked, and for the answer to be handled.
async fn asked(requests: &Requests) {
    util::eventually(async || {
        !requests
            .lock()
            .expect("no test panics holding the lock")
            .is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
}

#[tokio::test]
async fn good_ocsp_response_is_stapled() {
    let response = ocsp_response(&SERIAL, true, "20200101000000Z", Some("29990101000000Z"));
    let (address, requests) = responder(StatusCode::OK, response.clone()).await;
    let certificate = Certificate::new(Some(address));

    for tls_options in ["", "tls_options = { min_version = \"1.3\" }"] {
        let server = certificate.serve(tls_options);
        util::eventually(async || server.staple().await == response).await;
    }

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "one request per listener");
    let (content_type, request) = &requests[0];
    assert_eq!(content_type, "application/ocsp-request");
    // The request ends with the serial number of the certificate.
    assert!(
        request.ends_with(&[0x02, 0x03, 0x00, 0xab, 0xcd]),
        "{request:?}"
    );
}

#[tokio::test]
async fn ocsp_stapling_can_be_turned_off() {
    let response = ocsp_response(&SERIAL, true, "20200101000000Z", Some("29990101000000Z"));
    let (address, requests) = responder(StatusCode::OK, response).await;
    let certificate = Certificate::new(Some(address));
    let server = certificate.serve("tls_options = { ocsp_stapling = false }");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(server.staple().await.is_empty());
    assert!(requests.lock().unwrap().is_empty(), "nobody asked");
}

#[tokio::test]
async fn bad_ocsp_responses_are_not_stapled() {
    let good = ocsp_response(&SERIAL, true, "20200101000000Z", Some("29990101000000Z"));
    for (status, body) in [
        (
            StatusCode::OK,
            ocsp_response(&SERIAL, false, "20200101000000Z", None),
        ),
        (
            StatusCode::OK,
            ocsp_response(&SERIAL, true, "20200101000000Z", Some("20200102000000Z")),
        ),
        (
            StatusCode::OK,
            ocsp_response(&[0x01], true, "20200101000000Z", Some("29990101000000Z")),
        ),
        (StatusCode::OK, b"busy".to_vec()),
        (StatusCode::OK, vec![0x30; 100_000]),
        (StatusCode::INTERNAL_SERVER_ERROR, good),
    ] {
        let (address, requests) = responder(status, body).await;
        let server = Certificate::new(Some(address)).serve("");
        asked(&requests).await;
        assert!(server.staple().await.is_empty(), "{status}");
    }

    // A responder that is down does not keep the listener from serving.
    let server = Certificate::new(Some(util::dead_address())).serve("");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(server.staple().await.is_empty());
}

#[tokio::test]
async fn narrow_options_start_and_unusable_ones_are_refused() {
    // Only the TLS 1.3 suite is left, and the curve list is fine: starts.
    let certificate = Certificate::new(None);
    let options = "tls_options = { cipher_suites = [\"TLS_AES_128_GCM_SHA256\"], \
                   curves = [\"X25519MLKEM768\"] }";
    Master::from_config(&util::config(&certificate.config(options))).expect("master starts");

    // Validation refuses what no handshake could use.
    let options = "tls_options = { min_version = \"1.3\", \
                   cipher_suites = [\"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256\"] }";
    let err = rxh_server::config::Config::from_toml(&certificate.config(options)).unwrap_err();
    assert!(
        err.to_string().contains("needs a TLS 1.3 cipher suite"),
        "{err}"
    );
}
