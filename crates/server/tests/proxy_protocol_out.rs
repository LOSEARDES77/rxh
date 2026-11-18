//! PROXY protocol headers sent to the backends of `http` servers, and their
//! place before the TLS handshake of the upstream connection.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::client::conn::http1::SendRequest;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// What a [`proxied_backend`] saw, with the first line of the connection.
#[derive(Debug, PartialEq, Eq)]
enum Event {
    Opened(String),
    Closed(String),
}

async fn serve<S>(stream: S, line: String)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let service = service_fn(move |_| {
        let line = line.clone();
        async move { Ok::<_, Infallible>(Response::new(Full::<Bytes>::from(line))) }
    });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        .await;
}

/// HTTP backend that takes the first line of every connection as a PROXY
/// version 1 header, read in cleartext before the TLS handshake when
/// `acceptor` is set, and answers every request with that line.
async fn proxied_backend(
    acceptor: Option<TlsAcceptor>,
) -> (SocketAddr, mpsc::UnboundedReceiver<Event>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (events, receiver) = mpsc::unbounded_channel();
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let (events, acceptor) = (events.clone(), acceptor.clone());
            tokio::task::spawn(async move {
                // Byte by byte, so nothing after the header is consumed.
                let mut line = Vec::new();
                while !line.ends_with(b"\r\n") {
                    let Ok(byte) = stream.read_u8().await else {
                        return;
                    };
                    line.push(byte);
                }
                let line = String::from_utf8_lossy(&line).trim_end().to_owned();
                let _ = events.send(Event::Opened(line.clone()));
                match acceptor {
                    Some(acceptor) => match acceptor.accept(stream).await {
                        Ok(stream) => serve(stream, line.clone()).await,
                        Err(_) => return,
                    },
                    None => serve(stream, line.clone()).await,
                }
                let _ = events.send(Event::Closed(line));
            });
        }
    });
    (address, receiver)
}

fn tls_acceptor() -> TlsAcceptor {
    let certificate = rcgen::generate_simple_self_signed(vec![String::from("localhost")]).unwrap();
    let chain = vec![CertificateDer::from(certificate.cert.der().to_vec())];
    let key = PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

/// Keep-alive client connection to `proxy`, with its local address.
async fn client(proxy: SocketAddr) -> (SendRequest<Empty<Bytes>>, SocketAddr) {
    let stream = TcpStream::connect(proxy).await.unwrap();
    let local = stream.local_addr().unwrap();
    let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    (sender, local)
}

async fn get(sender: &mut SendRequest<Empty<Bytes>>) -> (StatusCode, String) {
    let request = Request::get("/")
        .header("host", "x.test")
        .body(Empty::new())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn next(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("the backend reports an event in time")
        .unwrap()
}

fn v1(client: SocketAddr, proxy: SocketAddr) -> String {
    format!(
        "PROXY TCP4 127.0.0.1 127.0.0.1 {} {}",
        client.port(),
        proxy.port()
    )
}

#[tokio::test]
async fn http_sends_v1_header_once_per_client_connection() {
    let (backend, mut events) = proxied_backend(None).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol = \"v1\"\nforward = \"{backend}\""
    ))[0];

    let (mut first, first_address) = client(proxy).await;
    let expected = v1(first_address, proxy);
    assert_eq!(get(&mut first).await, (StatusCode::OK, expected.clone()));
    assert_eq!(next(&mut events).await, Event::Opened(expected.clone()));

    // A second client must not travel on the connection announced for the first.
    let (mut second, second_address) = client(proxy).await;
    let other = v1(second_address, proxy);
    assert_eq!(get(&mut second).await, (StatusCode::OK, other.clone()));
    assert_eq!(next(&mut events).await, Event::Opened(other));

    // The first client still reuses its own upstream connection.
    assert_eq!(get(&mut first).await, (StatusCode::OK, expected));
    assert!(
        events.try_recv().is_err(),
        "no connection is opened or closed"
    );
}

#[tokio::test]
async fn http_sends_v2_header_and_fails_when_the_backend_hangs_up() {
    let (backend, mut captured) = util::tcp_capture(28).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol = \"v2\"\nforward = \"{backend}\""
    ))[0];

    let (mut sender, address) = client(proxy).await;
    // The capture backend closes the connection after the header.
    assert_eq!(get(&mut sender).await.0, StatusCode::BAD_GATEWAY);

    let mut expected = b"\r\n\r\n\0\r\nQUIT\n\x21\x11\x00\x0c".to_vec();
    expected.extend_from_slice(&[127, 0, 0, 1, 127, 0, 0, 1]);
    expected.extend_from_slice(&address.port().to_be_bytes());
    expected.extend_from_slice(&proxy.port().to_be_bytes());
    assert_eq!(captured.recv().await.unwrap(), expected);
}

#[tokio::test]
async fn http_without_proxy_protocol_sends_no_header() {
    let (backend, mut events) = proxied_backend(None).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\""
    ))[0];

    let (mut sender, _) = client(proxy).await;
    let _ = get(&mut sender).await;
    assert_eq!(
        next(&mut events).await,
        Event::Opened(String::from("GET / HTTP/1.1"))
    );
}

#[tokio::test]
async fn http_sends_header_before_the_upstream_tls_handshake() {
    let (backend, _events) = proxied_backend(Some(tls_acceptor())).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol = \"v1\"\n\
         forward = {{ backends = [\"https://{backend}\"], tls = {{ insecure_skip_verify = true }} }}"
    ))[0];

    let (mut sender, address) = client(proxy).await;
    assert_eq!(get(&mut sender).await, (StatusCode::OK, v1(address, proxy)));
}

#[tokio::test]
async fn tcp_sends_header_before_the_upstream_tls_handshake() {
    let (backend, _events) = proxied_backend(Some(tls_acceptor())).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nproxy_protocol = \"v1\"\n\
         forward = {{ backends = [\"{backend}\"], tls = {{ insecure_skip_verify = true }} }}"
    ))[0];

    let (mut sender, address) = client(proxy).await;
    assert_eq!(get(&mut sender).await, (StatusCode::OK, v1(address, proxy)));
}

#[tokio::test]
async fn http_forwards_the_source_of_an_incoming_header() {
    let (backend, _events) = proxied_backend(None).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol_in = true\n\
         proxy_protocol = \"v1\"\nforward = \"{backend}\""
    ))[0];

    let mut stream = TcpStream::connect(proxy).await.unwrap();
    stream
        .write_all(
            b"PROXY TCP4 203.0.113.7 127.0.0.1 40000 80\r\n\
              GET / HTTP/1.1\r\nhost: x.test\r\nconnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    let mut response = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut response))
        .await
        .expect("the proxy answers in time")
        .unwrap();
    let expected = format!("PROXY TCP4 203.0.113.7 127.0.0.1 40000 {}", proxy.port());
    assert!(response.ends_with(&expected), "{response:?}");
}

#[tokio::test]
async fn idle_connections_of_other_clients_are_closed_after_keepalive() {
    let (backend, mut events) = proxied_backend(None).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nproxy_protocol = \"v1\"\n\
         forward = {{ backends = [\"{backend}\"], keepalive = \"100ms\" }}"
    ))[0];

    let (mut first, first_address) = client(proxy).await;
    let line = v1(first_address, proxy);
    let _ = get(&mut first).await;
    assert_eq!(next(&mut events).await, Event::Opened(line.clone()));

    // Nothing asks for the first client's connection again: the next request
    // of anyone else has to drop it.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let (mut second, second_address) = client(proxy).await;
    let _ = get(&mut second).await;
    assert_eq!(
        next(&mut events).await,
        Event::Opened(v1(second_address, proxy))
    );
    assert_eq!(next(&mut events).await, Event::Closed(line));
}
