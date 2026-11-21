//! `[admin] tls`: the admin listener answers HTTPS, and plain HTTP to this
//! machine, on one port.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hyper::{Method, StatusCode};
use rxh_server::Daemon;
use rxh_test_util::free_address;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Writes a new self-signed certificate for `localhost` and its key to
/// `admin.pem` and `admin.key` of `dir`, and returns the certificate.
fn write_certificate(dir: &Path) -> CertificateDer<'static> {
    let generated = rcgen::generate_simple_self_signed(vec![String::from("localhost")])
        .expect("self-signed certificate");
    fs::write(dir.join("admin.pem"), generated.cert.pem()).expect("write certificate");
    fs::write(dir.join("admin.key"), generated.signing_key.serialize_pem()).expect("write key");
    generated.cert.der().clone()
}

/// Writes a configuration whose admin API listens on a free address with the
/// certificate files of `dir`, which need not exist.
fn write_config(dir: &Path) -> (PathBuf, SocketAddr) {
    let admin = free_address();
    let (cert, key) = (dir.join("admin.pem"), dir.join("admin.key"));
    let path = dir.join("rxh.toml");
    fs::write(
        &path,
        format!("[admin]\nlisten = \"{admin}\"\ntls = {{ cert = {cert:?}, key = {key:?} }}\n"),
    )
    .expect("write config");
    (path, admin)
}

/// A TLS connection to the admin API that trusts only `trusted`.
async fn connect(admin: SocketAddr, trusted: &[&CertificateDer<'static>]) -> TlsStream<TcpStream> {
    let mut roots = RootCertStore::empty();
    for certificate in trusted {
        roots
            .add((*certificate).clone())
            .expect("valid certificate");
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let stream = TcpStream::connect(admin).await.expect("admin accepts");
    let name = ServerName::try_from("localhost").expect("valid name");
    TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .expect("TLS handshake")
}

/// The certificate the admin API presents.
async fn served(
    admin: SocketAddr,
    trusted: &[&CertificateDer<'static>],
) -> CertificateDer<'static> {
    let stream = connect(admin, trusted).await;
    let (_, connection) = stream.get_ref();
    connection.peer_certificates().expect("certificates")[0].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn status_answers_over_tls() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let certificate = write_certificate(dir.path());
    let (config_path, admin) = write_config(dir.path());
    let _daemon = util::admin::start(&config_path, admin).await;

    let stream = connect(admin, &[&certificate]).await;
    let (status, _, json) =
        util::admin::call_on(stream, &admin.to_string(), Method::GET, "/status", "", &[]).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["admin"], admin.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn plain_http_from_this_machine_still_answers_on_a_tls_listener() {
    let dir = tempfile::tempdir().expect("temporary directory");
    write_certificate(dir.path());
    let (config_path, admin) = write_config(dir.path());
    let _daemon = util::admin::start(&config_path, admin).await;

    let (status, json) = util::admin::get(admin, "/status").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["admin"], admin.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn certificate_reload_serves_the_new_admin_certificate() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let first = write_certificate(dir.path());
    let (config_path, admin) = write_config(dir.path());
    let _daemon = util::admin::start(&config_path, admin).await;
    assert_eq!(served(admin, &[&first]).await, first);

    let second = write_certificate(dir.path());
    assert_ne!(first, second);
    let both = [&first, &second];
    let (status, json) = util::admin::send(admin, Method::POST, "/certificates/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let cert_file = dir.path().join("admin.pem");
    assert_eq!(
        json["reloaded"],
        serde_json::json!([cert_file.to_str().expect("UTF-8 path")])
    );
    assert_eq!(served(admin, &both).await, second);

    // A broken file is refused and the previous certificate stays.
    fs::write(&cert_file, "not a certificate").expect("break the certificate");
    let (status, json) = util::admin::send(admin, Method::POST, "/certificates/reload", "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(
        json["error"].as_str().expect("error").contains("admin.pem"),
        "{json}"
    );
    assert_eq!(served(admin, &both).await, second);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_admin_certificate_stops_the_start_with_the_file_name() {
    let dir = tempfile::tempdir().expect("temporary directory");
    let (config_path, admin) = write_config(dir.path());

    let outcome = tokio::time::timeout(Duration::from_secs(10), Daemon::new(&config_path).run())
        .await
        .expect("the daemon must not start without its admin certificate");
    let err = outcome.expect_err("no certificate file").to_string();
    assert!(err.contains("admin.pem"), "{err}");
    assert!(
        TcpStream::connect(admin).await.is_err(),
        "nothing listens after a failed start"
    );
}
