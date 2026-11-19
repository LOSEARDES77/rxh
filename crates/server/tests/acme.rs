//! `tls = "auto"`: stored certificates, runtime swaps through the resolver
//! and, against a local Pebble CA, the whole ACME flow with HTTP-01,
//! TLS-ALPN-01 and DNS-01 (a wildcard, through RFC 2136).

mod util;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rxh_server::{Daemon, Master};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};

/// A port that is free right now.
fn free_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address")
}

/// Accepts any server certificate; the tests compare it themselves.
#[derive(Debug)]
struct AnyCertificate;

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Handshakes with SNI `name`, trusting only `root` (any certificate if
/// `None`), then sends `GET /`. Returns the presented leaf certificate and
/// the response body.
async fn https_get(
    address: SocketAddr,
    name: &str,
    root: Option<CertificateDer<'static>>,
) -> Result<(CertificateDer<'static>, Bytes), String> {
    let client = match root {
        Some(root) => {
            let mut roots = RootCertStore::empty();
            roots.add(root).expect("valid root");
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth()
        }
        None => ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate))
            .with_no_client_auth(),
    };
    let stream = TcpStream::connect(address)
        .await
        .map_err(|err| err.to_string())?;
    let server_name = ServerName::try_from(name.to_owned()).expect("valid name");
    let tls = TlsConnector::from(Arc::new(client))
        .connect(server_name, stream)
        .await
        .map_err(|err| err.to_string())?;
    let presented = tls.get_ref().1.peer_certificates().expect("certificate")[0].clone();

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .expect("HTTP handshake");
    tokio::task::spawn(connection);
    let request = Request::get("/")
        .header("host", name)
        .body(Empty::<Bytes>::new())
        .expect("valid request");
    let response = sender.send_request(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    Ok((presented, body))
}

fn self_signed(name: &str) -> rcgen::CertifiedKey<rcgen::KeyPair> {
    rcgen::generate_simple_self_signed(vec![name.to_owned()]).expect("certificate")
}

fn write_stored(storage: &Path, name: &str) -> rcgen::CertifiedKey<rcgen::KeyPair> {
    let certificate = self_signed(name);
    let dir = storage.join(name);
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("cert.pem"), certificate.cert.pem()).expect("write cert");
    std::fs::write(dir.join("key.pem"), certificate.signing_key.serialize_pem())
        .expect("write key");
    certificate
}

#[tokio::test]
async fn certificates_set_at_runtime_are_served_without_restart() {
    let backend = util::backend("auto").await;
    let master = Master::from_config(&util::config(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nhost = \"a.test\"\ntls = \"auto\"\n\
         forward = \"{backend}\"\n"
    )))
    .unwrap();
    let address = master.sockets()[0];
    let resolvers = master.cert_resolvers(0);
    assert_eq!(resolvers.len(), 1, "one resolver per TLS listener");
    tokio::task::spawn(master.run());

    // No certificate yet: the handshake fails instead of using another one.
    assert!(https_get(address, "a.test", None).await.is_err());

    let certificate = self_signed("a.test");
    let key = resolvers[0]
        .certified_key(
            certificate.cert.pem().as_bytes(),
            certificate.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
    resolvers[0].set(&[String::from("a.test")], key).unwrap();

    let root = certificate.cert.der().clone();
    let (presented, body) = https_get(address, "a.test", Some(root.clone()))
        .await
        .unwrap();
    assert_eq!(presented, root);
    assert_eq!(body, "auto");
}

#[tokio::test]
async fn daemon_serves_stored_certificates_and_fails_missing_ones() {
    let backend = util::backend("stored").await;
    let dir = tempfile::tempdir().unwrap();
    // Relative storage: under the directory of the configuration file, one
    // directory per CA.
    let stored = write_stored(&dir.path().join("certs/127.0.0.1_1_directory"), "a.test");
    // A certificate of another CA (say staging) is not served.
    write_stored(&dir.path().join("certs/127.0.0.1_2_directory"), "b.test");
    let address = free_address();
    let http_port = free_address().port();
    let config = dir.path().join("rxh.toml");
    std::fs::write(
        &config,
        format!(
            "[admin]\nenabled = false\n\
             [acme]\nstorage = \"certs\"\nca = \"https://127.0.0.1:1/directory\"\n\
             http_port = {http_port}\n\
             [[server]]\nlisten = \"{address}\"\nhost = [\"a.test\", \"b.test\"]\n\
             tls = \"auto\"\nforward = \"{backend}\"\n"
        ),
    )
    .unwrap();
    let daemon = tokio::task::spawn(Daemon::new(&config).run());

    let root = stored.cert.der().clone();
    let mut served = None;
    for _ in 0..100 {
        if let Ok(result) = https_get(address, "a.test", Some(root.clone())).await {
            served = Some(result);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (presented, body) = served.expect("a.test is served from storage");
    assert_eq!(presented, root);
    assert_eq!(body, "stored");

    // b.test only has a certificate of another CA, and this CA is unreachable.
    assert!(https_get(address, "b.test", None).await.is_err());
    assert!(
        !daemon.is_finished(),
        "an ACME failure does not stop the daemon"
    );
    daemon.abort();
}

#[tokio::test]
async fn dns_01_serves_a_stored_wildcard_and_opens_no_challenge_listener() {
    let backend = util::backend("wildcard").await;
    let dns = rxh_test_util::DnsServer::spawn("127.0.0.1:0", "wild.test").await;
    let dir = tempfile::tempdir().unwrap();
    let ca_dir = dir.path().join("certs/127.0.0.1_1_directory");
    let stored = self_signed("*.wild.test");
    std::fs::create_dir_all(ca_dir.join("wildcard_.wild.test")).unwrap();
    std::fs::write(
        ca_dir.join("wildcard_.wild.test/cert.pem"),
        stored.cert.pem(),
    )
    .unwrap();
    std::fs::write(
        ca_dir.join("wildcard_.wild.test/key.pem"),
        stored.signing_key.serialize_pem(),
    )
    .unwrap();
    let address = free_address();
    let http_port = free_address().port();
    let config = dir.path().join("rxh.toml");
    std::fs::write(
        &config,
        format!(
            "[admin]\nenabled = false\n\
             [acme]\nstorage = \"certs\"\nca = \"https://127.0.0.1:1/directory\"\n\
             http_port = {http_port}\nchallenge = \"dns-01\"\n\
             [acme.dns]\nprovider = \"rfc2136\"\nserver = \"{}\"\npropagation_wait = \"0s\"\n\
             [[server]]\nlisten = \"{address}\"\nhost = [\"*.wild.test\", \"wild.test\"]\n\
             tls = \"auto\"\nhttps_redirect = false\nforward = \"{backend}\"\n",
            dns.address()
        ),
    )
    .unwrap();
    let daemon = tokio::task::spawn(Daemon::new(&config).run());

    // The stored wildcard certificate covers every subdomain.
    let root = stored.cert.der().clone();
    let mut served = None;
    for _ in 0..100 {
        if let Ok(result) = https_get(address, "any.wild.test", Some(root.clone())).await {
            served = Some(result);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (presented, body) = served.expect("any.wild.test is served from storage");
    assert_eq!(presented, root);
    assert_eq!(body, "wildcard");

    // The bare domain is another certificate, which this unreachable CA
    // never issues: nothing was published, and no port was opened for it.
    assert!(https_get(address, "wild.test", None).await.is_err());
    assert!(dns.updates().is_empty());
    assert!(
        std::net::TcpListener::bind(("0.0.0.0", http_port)).is_ok(),
        "DNS-01 opens no HTTP challenge listener"
    );
    assert!(!daemon.is_finished());
    daemon.abort();
}

/// Full issuance against [Pebble](https://github.com/letsencrypt/pebble).
///
/// ```sh
/// curl -o /tmp/pebble.minica.pem \
///   https://raw.githubusercontent.com/letsencrypt/pebble/main/test/certs/pebble.minica.pem
/// docker run --rm --network host --add-host acme.rxh.test:127.0.0.1 \
///   -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_WFE_NONCEREJECT=0 ghcr.io/letsencrypt/pebble
/// RXH_PEBBLE_ROOT=/tmp/pebble.minica.pem \
///   cargo test -p rxh-server --test acme -- --ignored
/// ```
///
/// Pebble listens on `https://localhost:14000/dir` and validates HTTP-01 on
/// port 5002, where nothing else listens, so RXH opens its challenge-only
/// listener there. `--add-host` makes `acme.rxh.test` resolve to this host
/// inside the container. Set `RXH_PEBBLE_DIRECTORY` for another directory URL.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Pebble ACME server, see the doc comment"]
async fn pebble_issues_and_installs_a_certificate() {
    pebble_issues("http-01", free_address()).await;
}

/// Same as [`pebble_issues_and_installs_a_certificate`] with TLS-ALPN-01,
/// which Pebble validates on port 5001: the TLS server itself listens there
/// and, with `https_redirect = false`, nothing listens on `http_port`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Pebble ACME server, see the doc comment"]
async fn pebble_issues_a_certificate_with_tls_alpn_01() {
    pebble_issues("tls-alpn-01", "127.0.0.1:5001".parse().unwrap()).await;
}

/// A wildcard certificate with DNS-01 through RFC 2136. Pebble must look the
/// TXT record up at the DNS server this test starts on port 8053, which takes
/// the dynamic updates of RXH:
///
/// ```sh
/// docker run --rm --network host -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_WFE_NONCEREJECT=0 \
///   ghcr.io/letsencrypt/pebble -dnsserver 127.0.0.1:8053
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a Pebble ACME server, see the doc comment"]
async fn pebble_issues_a_wildcard_certificate_with_dns_01() {
    let dns = rxh_test_util::DnsServer::spawn("127.0.0.1:8053", "rxh.test").await;
    pebble_issues("dns-01", free_address()).await;
    let updates = dns.updates();
    assert_eq!(updates.len(), 2, "the record is added, then deleted");
    assert!(dns.txt("_acme-challenge.rxh.test").is_empty());
}

async fn pebble_issues(challenge: &str, address: SocketAddr) {
    let root = std::env::var("RXH_PEBBLE_ROOT").expect("RXH_PEBBLE_ROOT: pebble.minica.pem path");
    let directory = std::env::var("RXH_PEBBLE_DIRECTORY")
        .unwrap_or_else(|_| String::from("https://localhost:14000/dir"));
    let backend = util::backend("pebble").await;
    let (host, stored, dns) = match challenge {
        "dns-01" => (
            "*.rxh.test",
            "wildcard_.rxh.test",
            "[acme.dns]\nprovider = \"rfc2136\"\nserver = \"127.0.0.1:8053\"\n\
             propagation_wait = \"0s\"\n",
        ),
        _ => ("acme.rxh.test", "acme.rxh.test", ""),
    };
    // The redirect of `tls = "auto"` would listen on `http_port` too: HTTP-01
    // is answered there, the others must work with nothing on that port.
    let redirect = if challenge != "http-01" {
        "https_redirect = false\n"
    } else {
        ""
    };
    let dir = tempfile::tempdir().expect("temporary directory");
    let config = dir.path().join("rxh.toml");
    std::fs::write(
        &config,
        format!(
            "[admin]\nenabled = false\n\
             [acme]\nca = {directory:?}\nca_root = {root:?}\nhttp_port = 5002\n\
             email = \"test@rxh.test\"\nchallenge = {challenge:?}\n{dns}\
             [[server]]\nlisten = \"{address}\"\nhost = \"{host}\"\n\
             tls = \"auto\"\n{redirect}forward = \"{backend}\"\n"
        ),
    )
    .expect("write the configuration");
    let daemon = tokio::task::spawn(Daemon::new(&config).run());

    let storage = dir.path().join("rxh-data/certificates");
    let ca_dir = directory.trim_start_matches("https://").replace(
        |c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'),
        "_",
    );
    let cert = storage.join(&ca_dir).join(stored).join("cert.pem");
    for _ in 0..600 {
        if cert.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        cert.exists(),
        "no certificate after 60 seconds, see the daemon log"
    );
    assert!(storage.join(&ca_dir).join(stored).join("key.pem").exists());
    let accounts = std::fs::read_dir(storage.join("accounts"))
        .expect("accounts directory")
        .count();
    assert_eq!(accounts, 1, "the account is stored");

    // The new certificate is served without a restart.
    let pem = std::fs::read(&cert).expect("read the certificate");
    let leaf = CertificateDer::pem_slice_iter(&pem)
        .next()
        .expect("a certificate")
        .expect("valid PEM");
    let mut served = None;
    for _ in 0..50 {
        if let Ok(result) = https_get(address, &host.replace('*', "any"), None).await {
            served = Some(result);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (presented, body) = served.expect("handshake with the issued certificate");
    assert_eq!(presented, leaf);
    assert_eq!(body, "pebble");
    if challenge != "http-01" {
        // Checked while the daemon still runs, after the whole issuance.
        assert!(
            std::net::TcpListener::bind("0.0.0.0:5002").is_ok(),
            "only HTTP-01 opens an HTTP challenge listener"
        );
    }
    daemon.abort();
}
