//! HTTP/1.1 backend that fails in one chosen way, for tests of the error paths of a proxy.

use std::mem;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

/// Body of the response sent by [`Fault::SlowHeaders`] once its delay passed.
pub const SLOW_BODY: &str = "slow";

/// Response sent by [`Fault::TruncatedBody`]: it announces 1000 body bytes and sends 10.
pub const TRUNCATED_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 1000\r\n\r\nonly ten b";

/// How a [`FlakyBackend`] misbehaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Connections are refused: the port is bound but nothing listens on it.
    Refuse,
    /// Each connection is accepted and closed at once, before any byte is read or written.
    AcceptThenClose,
    /// The request head is read, then the response head waits for the delay. A complete
    /// `200` response with [`SLOW_BODY`] follows, and the connection is closed.
    SlowHeaders(Duration),
    /// The request head is read, then [`TRUNCATED_RESPONSE`] is sent and the connection is
    /// closed in the middle of the body.
    TruncatedBody,
}

/// Backend listening on a localhost port and failing as its [`Fault`] says.
#[derive(Debug)]
pub struct FlakyBackend {
    address: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl FlakyBackend {
    /// Starts a backend on a free localhost port. Must run inside a Tokio runtime.
    pub async fn spawn(fault: Fault) -> Self {
        let connections = Arc::new(AtomicUsize::new(0));
        let address = match fault {
            Fault::Refuse => refusing_address(),
            Fault::AcceptThenClose | Fault::SlowHeaders(_) | Fault::TruncatedBody => {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let address = listener.local_addr().expect("address");
                tokio::task::spawn(accept_loop(listener, fault, Arc::clone(&connections)));
                address
            }
        };
        Self {
            address,
            connections,
        }
    }

    /// Address to give the proxy as a backend.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// Number of connections accepted so far (always 0 for [`Fault::Refuse`]).
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// A localhost address whose port is bound but not listening.
fn refusing_address() -> SocketAddr {
    let socket = TcpSocket::new_v4().expect("socket");
    socket
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind");
    let address = socket.local_addr().expect("address");
    // The port stays taken for the whole test binary, so a listener of a concurrent test cannot
    // be given it and turn the refusal into an answer.
    mem::forget(socket);
    address
}

async fn accept_loop(listener: TcpListener, fault: Fault, connections: Arc<AtomicUsize>) {
    while let Ok((stream, _)) = listener.accept().await {
        connections.fetch_add(1, Ordering::SeqCst);
        tokio::task::spawn(misbehave(stream, fault));
    }
}

async fn misbehave(mut stream: TcpStream, fault: Fault) {
    match fault {
        Fault::Refuse | Fault::AcceptThenClose => {}
        Fault::SlowHeaders(delay) => {
            read_head(&mut stream).await;
            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{SLOW_BODY}",
                SLOW_BODY.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
        Fault::TruncatedBody => {
            read_head(&mut stream).await;
            let _ = stream.write_all(TRUNCATED_RESPONSE).await;
        }
    }
}

/// Reads until the end of the request head, or until the peer stops sending.
async fn read_head(stream: &mut TcpStream) {
    let mut received = Vec::new();
    let mut buffer = [0; 4096];
    while !received.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut buffer).await {
            Ok(read @ 1..) => received.extend_from_slice(&buffer[..read]),
            _ => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::ErrorKind;
    use std::time::Instant;

    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: x\r\n\r\n";

    /// Sends [`REQUEST`] to `backend` and returns everything it answers until it closes.
    async fn exchange(backend: &FlakyBackend) -> Vec<u8> {
        let mut stream = TcpStream::connect(backend.address()).await.unwrap();
        // Ignored: an accept-then-close backend may already be gone.
        let _ = stream.write_all(REQUEST).await;
        let mut received = Vec::new();
        let _ = stream.read_to_end(&mut received).await;
        received
    }

    #[tokio::test]
    async fn refuse_refuses_connections() {
        let backend = FlakyBackend::spawn(Fault::Refuse).await;
        let error = TcpStream::connect(backend.address()).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::ConnectionRefused);
        assert_eq!(backend.connections(), 0);
    }

    #[tokio::test]
    async fn accept_then_close_answers_nothing() {
        let backend = FlakyBackend::spawn(Fault::AcceptThenClose).await;
        assert!(exchange(&backend).await.is_empty());
        assert_eq!(backend.connections(), 1);
    }

    #[tokio::test]
    async fn slow_headers_answer_after_the_delay() {
        let backend = FlakyBackend::spawn(Fault::SlowHeaders(Duration::from_millis(300))).await;
        let started = Instant::now();
        let received = exchange(&backend).await;
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(received.starts_with(b"HTTP/1.1 200 OK\r\n"));
        assert!(received.ends_with(b"\r\n\r\nslow"));
    }

    #[tokio::test]
    async fn truncated_body_stops_short_of_its_length() {
        let backend = FlakyBackend::spawn(Fault::TruncatedBody).await;
        assert_eq!(exchange(&backend).await, TRUNCATED_RESPONSE);
    }
}
