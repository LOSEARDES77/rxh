//! A stand-in for the HTTP APIs of Consul, etcd and Nomad, for the tests of
//! the service catalog providers.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Answers each request with the JSON body the test published for its
/// method and path (query included), or `404` without one, on a local TCP
/// address.
#[derive(Debug)]
pub struct CatalogApi {
    address: SocketAddr,
    state: Arc<State>,
}

#[derive(Debug, Default)]
struct State {
    /// Bodies by `METHOD /path?query`.
    bodies: Mutex<HashMap<String, String>>,
    /// Request heads received, in order.
    requests: Mutex<Vec<String>>,
}

impl CatalogApi {
    /// Starts the API on a free local port, answering nothing.
    ///
    /// # Panics
    ///
    /// Panics if no socket can be bound to localhost.
    pub async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(State::default());
        let shared = Arc::clone(&state);
        tokio::task::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::task::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self { address, state }
    }

    /// The `endpoint` to give to `[consul]`, `[etcd]` or `[nomad]`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    /// Answers `request`, such as `GET /v1/catalog/services`, with `body`.
    pub fn publish(&self, request: &str, body: &str) {
        self.state
            .bodies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(request.to_owned(), body.to_owned());
    }

    /// Heads of the requests received so far, header names in lower case.
    #[must_use]
    pub fn requests(&self) -> Vec<String> {
        self.state
            .requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let mut request: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => return,
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).to_lowercase();
    // Read the body too, so closing does not reset the connection.
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while request.len() < head_end + length {
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => return,
        }
    }
    let line = String::from_utf8_lossy(&request[..head_end])
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let key = line
        .rsplit_once(' ')
        .map_or(line.as_str(), |(key, _)| key)
        .to_owned();
    state
        .requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(head);

    let body = state
        .bodies
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .cloned();
    let (status, body) = match body {
        Some(body) => ("200 OK", body),
        None => ("404 Not Found", String::from("{}")),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}
