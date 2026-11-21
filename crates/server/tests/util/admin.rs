//! A client of the admin API and a daemon started from a configuration file.

use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::Daemon;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

/// Sends one request to the admin API and returns status, headers and the
/// body parsed as JSON (`Value::Null` when it is not JSON).
pub async fn call(
    admin: SocketAddr,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let stream = TcpStream::connect(admin).await.expect("admin accepts");
    call_on(stream, &admin.to_string(), method, uri, body, headers).await
}

/// [`call`] on an open connection, TCP or unix socket.
pub async fn call_on<S>(
    stream: S,
    host: &str,
    method: Method,
    uri: &str,
    body: &str,
    headers: &[(header::HeaderName, &str)],
) -> (StatusCode, HeaderMap, Value)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let mut request = Request::builder().method(method).uri(uri);
    if !headers.iter().any(|(name, _)| name == header::HOST) {
        request = request.header(header::HOST, host);
    }
    for (name, value) in headers {
        request = request.header(name, *value);
    }
    let request = request
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("valid request");
    let response = sender.send_request(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&body).unwrap_or(Value::Null);
    (parts.status, parts.headers, json)
}

/// `GET` on the admin API: status and JSON body.
pub async fn get(admin: SocketAddr, uri: &str) -> (StatusCode, Value) {
    let (status, _, json) = call(admin, Method::GET, uri, "", &[]).await;
    (status, json)
}

/// Any method with a body on the admin API: status and JSON body.
pub async fn send(admin: SocketAddr, method: Method, uri: &str, body: &str) -> (StatusCode, Value) {
    let (status, _, json) = call(admin, method, uri, body, &[]).await;
    (status, json)
}

/// Starts a daemon on the configuration file and waits for its admin API.
pub async fn start(
    config_path: &Path,
    admin: SocketAddr,
) -> JoinHandle<Result<(), rxh_server::Error>> {
    let daemon = tokio::task::spawn(Daemon::new(config_path).run());
    super::eventually(async || TcpStream::connect(admin).await.is_ok()).await;
    daemon
}
