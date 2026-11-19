//! Minimal client for the daemon's admin API.

use std::fmt;
use std::io;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Method;
use hyper::Request;
use hyper::StatusCode;
use hyper::header;
use hyper_util::rt::TokioIo;
use rxh_core::config::AdminListen;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Why an admin request failed.
#[derive(Debug)]
pub(crate) enum ClientError {
    /// No daemon answered on the admin address.
    Unreachable(io::Error),

    /// The daemon answered with an error status.
    Api { status: StatusCode, message: String },

    /// The answer was not valid JSON or HTTP.
    Protocol(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Unreachable(err) => write!(f, "daemon is not reachable: {err}"),
            ClientError::Api { status, message } => write!(f, "{message} ({status})"),
            ClientError::Protocol(message) => write!(f, "bad answer from daemon: {message}"),
        }
    }
}

/// Admin API client.
#[derive(Debug, Clone)]
pub(crate) struct AdminClient {
    address: AdminListen,
    token: Option<String>,
}

impl AdminClient {
    /// Client of the daemon on `address`, sending `token` as a bearer token.
    pub(crate) fn new(address: AdminListen, token: Option<String>) -> Self {
        Self { address, token }
    }

    pub(crate) fn address(&self) -> &AdminListen {
        &self.address
    }

    pub(crate) async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.request(Method::GET, path, None).await
    }

    pub(crate) async fn post(&self, path: &str, body: Option<Value>) -> Result<Value, ClientError> {
        self.request(Method::POST, path, body).await
    }

    pub(crate) async fn delete(&self, path: &str, body: Value) -> Result<Value, ClientError> {
        self.request(Method::DELETE, path, Some(body)).await
    }

    /// Body of a `GET` answered with text, like `/metrics`.
    pub(crate) async fn get_text(&self, path: &str) -> Result<String, ClientError> {
        let (status, bytes) = self.timed(Method::GET, path, None).await?;
        if status.is_success() {
            String::from_utf8(bytes.to_vec())
                .map_err(|err| ClientError::Protocol(format!("invalid UTF-8: {err}")))
        } else {
            Err(api_error(status, &bytes))
        }
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ClientError> {
        let (status, bytes) = self.timed(method, path, body).await?;
        if !status.is_success() {
            return Err(api_error(status, &bytes));
        }
        serde_json::from_slice(&bytes)
            .map_err(|err| ClientError::Protocol(format!("invalid JSON: {err}")))
    }

    async fn timed(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Bytes), ClientError> {
        tokio::time::timeout(TIMEOUT, self.send(method, path, body))
            .await
            .map_err(|_| ClientError::Protocol(String::from("timed out")))?
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Bytes), ClientError> {
        match &self.address {
            AdminListen::Tcp(address) => {
                let stream = TcpStream::connect(address)
                    .await
                    .map_err(ClientError::Unreachable)?;
                let host = format!("localhost:{}", address.port());
                self.exchange(stream, &host, method, path, body).await
            }
            #[cfg(unix)]
            AdminListen::Unix(socket) => {
                let stream = tokio::net::UnixStream::connect(socket)
                    .await
                    .map_err(ClientError::Unreachable)?;
                self.exchange(stream, "localhost", method, path, body).await
            }
            #[cfg(not(unix))]
            AdminListen::Unix(_) => Err(ClientError::Unreachable(io::Error::new(
                io::ErrorKind::Unsupported,
                "unix sockets are not available on this system",
            ))),
        }
    }

    async fn exchange<S>(
        &self,
        stream: S,
        host: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Bytes), ClientError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| ClientError::Protocol(err.to_string()))?;
        tokio::task::spawn(connection);

        let body = body.map(|value| value.to_string()).unwrap_or_default();
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = &self.token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| ClientError::Protocol(err.to_string()))?;

        let response = sender
            .send_request(request)
            .await
            .map_err(|err| ClientError::Protocol(err.to_string()))?;
        let status = response.status();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|err| ClientError::Protocol(err.to_string()))?
            .to_bytes();
        Ok((status, bytes))
    }
}

/// The error of a failed answer: the `error` field of its JSON body.
fn api_error(status: StatusCode, body: &[u8]) -> ClientError {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| value["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| String::from("unknown error"));
    ClientError::Api { status, message }
}
