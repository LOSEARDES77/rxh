//! Certificate files read again without a restart, and the admin routes
//! `GET /certificates` and `POST /certificates/reload`.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::Daemon;
use rxh_test_util::{OcspCertificate, free_address};
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// Sends one request to the admin API and returns the status and the JSON.
async fn admin(address: SocketAddr, method: Method, uri: &str) -> (StatusCode, Value) {
    let stream = TcpStream::connect(address).await.expect("admin accepts");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, address.to_string())
        .body(Full::new(Bytes::new()))
        .expect("valid request");
    let response = sender.send_request(request).await.expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, serde_json::from_slice(&body).expect("JSON"))
}

/// End entity certificate of a PEM chain.
fn end_entity(chain_pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(chain_pem.as_bytes()).expect("PEM certificate")
}

fn write(dir: &Path, certificate: &OcspCertificate) {
    fs::write(dir.join("chain.pem"), &certificate.chain_pem).expect("write chain");
    fs::write(dir.join("key.pem"), &certificate.key_pem).expect("write key");
}

/// End entity certificate the listener at `address` presents for
/// `tls.test`, trusting the authorities of `certificates`.
async fn served(address: SocketAddr, certificates: &[&OcspCertificate]) -> CertificateDer<'static> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates {
        roots
            .add(certificate.authority_der.clone().into())
            .expect("valid authority");
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let stream = TcpStream::connect(address).await.expect("listener accepts");
    let name = ServerName::try_from("tls.test").expect("valid name");
    let stream = TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await
        .expect("handshake");
    let (_, connection) = stream.get_ref();
    connection.peer_certificates().expect("certificates")[0].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn certificate_files_reload_without_restart() {
    let first = OcspCertificate::new(None);
    let second = OcspCertificate::new(None);
    let both = [&first, &second];
    let dir = tempfile::tempdir().expect("temporary directory");
    write(dir.path(), &first);
    let (admin_address, web) = (free_address(), free_address());
    let config_path = dir.path().join("rxh.toml");
    let cert = dir.path().join("chain.pem");
    let key = dir.path().join("key.pem");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin_address}\"\n\
             [[server]]\nlisten = \"{web}\"\nprotocol = \"tcp\"\nhost = \"tls.test\"\n\
             forward = \"{}\"\ntls = {{ cert = {cert:?}, key = {key:?} }}\n",
            util::dead_address()
        ),
    )
    .expect("write config");
    let _daemon = tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin_address).await.is_ok()).await;
    util::eventually(async || TcpStream::connect(web).await.is_ok()).await;
    assert_eq!(served(web, &both).await, end_entity(&first.chain_pem));

    // Listed with its files and expiry.
    let (status, list) = admin(admin_address, Method::GET, "/certificates").await;
    assert_eq!(status, StatusCode::OK);
    let entry = &list[0];
    assert_eq!(list.as_array().expect("array").len(), 1, "{list}");
    assert_eq!(entry["listen"], web.to_string());
    assert_eq!(entry["hosts"], serde_json::json!(["tls.test"]));
    assert_eq!(entry["names"], serde_json::json!(["tls.test"]));
    assert_eq!(entry["default"], true);
    assert_eq!(entry["source"], "file");
    assert_eq!(entry["cert_file"], cert.to_str().expect("UTF-8 path"));
    assert_eq!(entry["key_file"], key.to_str().expect("UTF-8 path"));
    assert!(entry["days_left"].as_i64().expect("days") > 0, "{entry}");
    assert!(entry["not_after"].as_str().expect("date").ends_with('Z'));

    // New files are picked up on their own, the listener keeps running.
    write(dir.path(), &second);
    let mut reloaded = false;
    for _ in 0..60 {
        if served(web, &both).await == end_entity(&second.chain_pem) {
            reloaded = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(reloaded, "the new certificate is served within 12s");

    // A broken file is refused and the previous certificate stays.
    fs::write(&cert, "not a certificate").expect("break the chain");
    let (status, json) = admin(admin_address, Method::POST, "/certificates/reload").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert!(
        json["error"].as_str().expect("error").contains("chain.pem"),
        "{json}"
    );
    assert_eq!(served(web, &both).await, end_entity(&second.chain_pem));

    // A forced reload reads unchanged files too.
    write(dir.path(), &first);
    let (status, json) = admin(admin_address, Method::POST, "/certificates/reload").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(
        json["reloaded"],
        serde_json::json!([cert.to_str().expect("UTF-8 path")])
    );
    assert_eq!(served(web, &both).await, end_entity(&first.chain_pem));
}

#[tokio::test(flavor = "multi_thread")]
async fn no_tls_listener_means_no_certificates() {
    let admin_address = free_address();
    let dir = tempfile::tempdir().expect("temporary directory");
    let config_path = dir.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin_address}\"\n\
             [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{}\"\n",
            util::dead_address()
        ),
    )
    .expect("write config");
    let _daemon = tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin_address).await.is_ok()).await;

    let (status, list) = admin(admin_address, Method::GET, "/certificates").await;
    assert_eq!((status, list), (StatusCode::OK, serde_json::json!([])));
    let (status, json) = admin(admin_address, Method::POST, "/certificates/reload").await;
    assert_eq!(
        (status, json),
        (StatusCode::OK, serde_json::json!({ "reloaded": [] }))
    );
}
