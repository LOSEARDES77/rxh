//! Several `[[server]]` entries sharing one listen address, routed by host
//! name and, with TLS, by SNI.

mod util;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rcgen::CertifiedKey;
use rxh_server::Master;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

/// A port that is free right now. Port 0 cannot be used because both
/// servers must bind the same address.
fn free_address() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("address")
}

async fn get_host(address: SocketAddr, host: &str) -> (StatusCode, Bytes) {
    let request = Request::get("/")
        .header("host", host)
        .body(Empty::new())
        .expect("valid request");
    let (status, _, body) = util::send(address, request).await;
    (status, body)
}

#[tokio::test]
async fn servers_on_one_address_route_by_host() {
    let a = util::backend("site a").await;
    let b = util::backend("site b").await;
    let address = free_address();

    let master = Master::from_config(&util::config(&format!(
        "[[server]]\nname = \"a\"\nlisten = \"{address}\"\nhost = \"a.test\"\nforward = \"{a}\"\n\
         [[server]]\nname = \"b\"\nlisten = \"{address}\"\nhost = \"b.test\"\nforward = \"{b}\"\n"
    )))
    .unwrap();
    // One entry per socket, as in the admin `/status`.
    let servers = master.servers();
    assert_eq!(servers.len(), 1, "both servers share one socket");
    assert_eq!(servers[0].address, address);
    assert_eq!(servers[0].name.as_deref(), Some("a, b"));
    tokio::task::spawn(master.run());

    for _ in 0..4 {
        assert_eq!(
            get_host(address, "a.test").await,
            (StatusCode::OK, "site a".into())
        );
        assert_eq!(
            get_host(address, "b.test:80").await,
            (StatusCode::OK, "site b".into())
        );
    }
    assert_eq!(get_host(address, "c.test").await.0, StatusCode::NOT_FOUND);
}

fn write_certificate(dir: &Path, host: &str) -> (CertifiedKey<rcgen::KeyPair>, String) {
    let certificate =
        rcgen::generate_simple_self_signed(vec![host.to_owned()]).expect("certificate");
    let cert = dir.join(format!("{host}.pem"));
    let key = dir.join(format!("{host}.key"));
    std::fs::write(&cert, certificate.cert.pem()).expect("write cert");
    std::fs::write(&key, certificate.signing_key.serialize_pem()).expect("write key");
    let tls = format!(
        "tls = {{ cert = {:?}, key = {:?} }}",
        cert.to_str().expect("UTF-8 path"),
        key.to_str().expect("UTF-8 path")
    );
    (certificate, tls)
}

#[tokio::test]
async fn sni_selects_the_certificate() {
    let a = util::backend("secure a").await;
    let b = util::backend("secure b").await;
    let dir = tempfile::tempdir().unwrap();
    let (_, tls_a) = write_certificate(dir.path(), "a.test");
    let (cert_b, tls_b) = write_certificate(dir.path(), "b.test");
    let address = free_address();

    util::spawn(&format!(
        "[[server]]\nlisten = \"{address}\"\nhost = \"a.test\"\nforward = \"{a}\"\n{tls_a}\n\
         [[server]]\nlisten = \"{address}\"\nhost = \"b.test\"\nforward = \"{b}\"\n{tls_b}\n"
    ));

    // Only the b.test certificate is trusted, so the handshake fails unless
    // the proxy picks it by SNI.
    let mut roots = RootCertStore::empty();
    roots.add(cert_b.cert.der().clone()).unwrap();
    let client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(client));

    let stream = TcpStream::connect(address).await.unwrap();
    let name = ServerName::try_from("b.test").unwrap();
    let tls = connector.connect(name, stream).await.unwrap();
    let presented = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
    assert_eq!(presented, *cert_b.cert.der());

    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let request = Request::get("/")
        .header("host", "b.test")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "secure b"
    );
}
