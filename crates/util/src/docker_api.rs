//! A stand-in for the Docker Engine API, for the tests of the Docker provider.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Serves `GET /containers/json` and `GET /events`, and the Swarm
/// `GET /services` and `GET /tasks`, on a local TCP address, with whatever
/// the test publishes.
#[derive(Debug)]
pub struct DockerApi {
    address: SocketAddr,
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    containers: Mutex<String>,
    /// `GET /services` and `GET /tasks`.
    swarm: Mutex<(String, String)>,
    events: broadcast::Sender<String>,
}

impl DockerApi {
    /// Starts the API on a free local port, without containers.
    ///
    /// # Panics
    ///
    /// Panics if no socket can be bound to localhost.
    pub async fn start() -> Self {
        Self::start_on(SocketAddr::from(([127, 0, 0, 1], 0))).await
    }

    /// Starts the API on `address`, without containers.
    ///
    /// # Panics
    ///
    /// Panics if `address` cannot be bound.
    pub async fn start_on(address: SocketAddr) -> Self {
        let listener = TcpListener::bind(address).await.expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(State {
            containers: Mutex::new(String::from("[]")),
            swarm: Mutex::new((String::from("[]"), String::from("[]"))),
            events: broadcast::channel(64).0,
        });
        let shared = Arc::clone(&state);
        tokio::task::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::task::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self { address, state }
    }

    /// The `endpoint` to give to `[docker]`.
    #[must_use]
    pub fn endpoint(&self) -> String {
        format!("tcp://{}", self.address)
    }

    /// Makes `containers`, a JSON array as `GET /containers/json` returns
    /// it, the container list, then sends the event `action` (`start` or
    /// `die`) of the container `id` to every event stream.
    pub fn publish(&self, containers: &str, action: &str, id: &str) {
        *self
            .state
            .containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = containers.to_owned();
        let event = format!(
            "{{\"Type\":\"container\",\"Action\":\"{action}\",\"Actor\":{{\"ID\":\"{id}\"}}}}\n"
        );
        // No event stream is open yet: the next one starts with the list.
        let _ = self.state.events.send(event);
    }
}

impl DockerApi {
    /// Makes `services` and `tasks`, JSON arrays as `GET /services` and
    /// `GET /tasks` return them, the Swarm lists.
    pub fn publish_swarm(&self, services: &str, tasks: &str) {
        *self
            .state
            .swarm
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = (services.to_owned(), tasks.to_owned());
    }
}

async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let mut request: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => return,
        }
    }
    let request = String::from_utf8_lossy(&request);
    let path = request.split(' ').nth(1).unwrap_or_default();

    if path.starts_with("/events") {
        // Subscribed before the response starts, as Docker does.
        let mut events = state.events.subscribe();
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                    Transfer-Encoding: chunked\r\n\r\n";
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        while let Ok(event) = events.recv().await {
            let chunk = format!("{:x}\r\n{event}\r\n", event.len());
            if stream.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
        }
        return;
    }

    let (status, body) = if path.starts_with("/containers/json") {
        let containers = state
            .containers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        ("200 OK", containers.clone())
    } else if path.starts_with("/services") || path.starts_with("/tasks") {
        let swarm = state.swarm.lock().unwrap_or_else(PoisonError::into_inner);
        let body = if path.starts_with("/services") {
            &swarm.0
        } else {
            &swarm.1
        };
        ("200 OK", body.clone())
    } else {
        (
            "404 Not Found",
            String::from("{\"message\":\"page not found\"}"),
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}
