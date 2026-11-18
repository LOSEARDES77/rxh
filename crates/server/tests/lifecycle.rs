//! Listener lifecycle: graceful shutdown, connection limits, TLS, files.

mod util;

use std::io;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::header;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use rxh_server::{ShutdownState, State};
use tokio::net::{TcpSocket, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[tokio::test]
async fn graceful_shutdown_serves_open_connections() {
    let backend = util::backend("Hello world").await;
    let (proxy, shutdown, mut state) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nuri = \"/hello\"\nforward = \"{backend}\""
    ));

    state
        .wait_for(|state| *state == State::Listening)
        .await
        .unwrap();

    let stream1 = TcpStream::connect(proxy).await.unwrap();
    let stream2 = TcpStream::connect(proxy).await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    shutdown();
    state
        .wait_for(|state| *state == State::ShuttingDown(ShutdownState::PendingConnections(2)))
        .await
        .unwrap();

    let refused = TcpSocket::new_v4()
        .unwrap()
        .connect(proxy)
        .await
        .err()
        .unwrap();
    assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);

    for stream in [stream1, stream2] {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::task::spawn(connection);
        let request = Request::get("/hello").body(Empty::<Bytes>::new()).unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            "Hello world"
        );
    }

    state
        .wait_for(|state| *state == State::ShuttingDown(ShutdownState::Done))
        .await
        .unwrap();
}

#[tokio::test]
async fn keep_alive_connections_do_not_block_shutdown() {
    let backend = util::backend("ok").await;
    let (proxy, shutdown, mut state) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ));

    let stream = TcpStream::connect(proxy).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let response = sender
        .send_request(Request::get("/").body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();

    // The client keeps `sender` alive, the connection is idle keep-alive.
    shutdown();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.wait_for(|state| *state == State::ShuttingDown(ShutdownState::Done)),
    )
    .await
    .expect("idle keep-alive connection must be closed on shutdown")
    .unwrap();
    drop(sender);
}

#[tokio::test]
async fn first_request_during_shutdown_still_closes_the_connection() {
    let backend = util::backend("ok").await;
    let (proxy, shutdown, mut state) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ));
    state
        .wait_for(|state| *state == State::Listening)
        .await
        .unwrap();

    let stream = TcpStream::connect(proxy).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    // Let the server accept the connection before the shutdown.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Shutdown starts while the connection has not sent a request yet.
    shutdown();
    state
        .wait_for(|state| *state == State::ShuttingDown(ShutdownState::PendingConnections(1)))
        .await
        .unwrap();

    // Its first request is answered, then the connection is closed although
    // the client keeps `sender` alive.
    let response = sender
        .send_request(Request::get("/").body(Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap();
    response.into_body().collect().await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        state.wait_for(|state| *state == State::ShuttingDown(ShutdownState::Done)),
    )
    .await
    .expect("the connection must close after its first request")
    .unwrap();
    drop(sender);
}

#[tokio::test]
async fn max_connections_is_enforced() {
    let backend = util::backend("ok").await;
    let (proxy, _shutdown, mut state) = util::spawn_controlled(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmax_connections = 2\nforward = \"{backend}\""
    ));

    let first = TcpStream::connect(proxy).await.unwrap();
    let _second = TcpStream::connect(proxy).await.unwrap();
    state
        .wait_for(|state| *state == State::MaxConnectionsReached(2))
        .await
        .unwrap();

    drop(first);
    state
        .wait_for(|state| *state == State::Listening)
        .await
        .unwrap();
}

#[tokio::test]
async fn tls_listener_accepts_https() {
    let backend = util::backend("tls-ok").await;
    let dir = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec![String::from("localhost")]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, certificate.cert.pem()).unwrap();
    std::fs::write(&key_path, certificate.signing_key.serialize_pem()).unwrap();

    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\ntls = {{ cert = {:?}, key = {:?} }}",
        cert_path.to_str().unwrap(),
        key_path.to_str().unwrap()
    ))[0];

    let mut roots = RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let mut client = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(std::sync::Arc::new(client));

    let stream = TcpStream::connect(proxy).await.unwrap();
    let name = tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(name, stream).await.unwrap();
    assert_eq!(
        tls.get_ref().1.alpn_protocol(),
        Some(b"h2".as_slice()),
        "h2 must be negotiated"
    );

    let (mut sender, connection) =
        hyper::client::conn::http2::Builder::new(hyper_util::rt::TokioExecutor::new())
            .handshake(TokioIo::new(tls))
            .await
            .unwrap();
    tokio::task::spawn(connection);
    let response = sender
        .send_request(
            Request::get("https://localhost/")
                .body(Empty::<Bytes>::new())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.into_body().collect().await.unwrap().to_bytes(),
        "tls-ok"
    );
}

#[tokio::test]
async fn static_files_and_explorer() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.txt"), "prefixed-file").unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub").join("hello.txt"), "hello").unwrap();
    let root = dir.path().to_str().unwrap();

    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nuri = \"/site\"\nserve = {root:?}"
    ))[0];

    let (status, headers, body) = util::get(proxy, "/site/index.txt").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "prefixed-file");
    assert!(
        headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );

    let (status, _, body) = util::get(proxy, "/site/sub").await;
    assert_eq!(status, StatusCode::OK);
    let page = String::from_utf8_lossy(&body);
    assert!(page.contains("href=\"/site/sub/hello.txt\""), "{page}");

    let (status, _, body) = util::get(proxy, "/site/missing.css").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("404"));

    let (status, _, _) = util::get(proxy, "/site/../../etc/passwd").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn multiple_servers_share_one_master() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "Hello World File").unwrap();
    let backend = util::backend("Hello World Response").await;
    let sockets = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nuri = \"/api\"\nforward = \"{backend}\"\n\
         [[server]]\nlisten = [\"127.0.0.1:0\", \"127.0.0.1:0\"]\nserve = {:?}",
        dir.path().to_str().unwrap()
    ));

    assert_eq!(sockets.len(), 3);
    assert_eq!(
        util::get(sockets[0], "/api").await.2,
        "Hello World Response"
    );
    assert_eq!(
        util::get(sockets[1], "/test.txt").await.2,
        "Hello World File"
    );
    assert_eq!(
        util::get(sockets[2], "/test.txt").await.2,
        "Hello World File"
    );
}
