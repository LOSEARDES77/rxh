//! HTTP/3: a QUIC endpoint next to the TCP listener of every `http` server
//! with TLS.
//!
//! A [`Listener`] is what one generation serves on one address: routes,
//! limits, log and QUIC TLS settings, shared with its TCP listener. The
//! daemon owns the [`Endpoints`], one UDP socket per address, and they
//! outlive generations: two sockets on one port would split the packets of
//! every connection between them. Installing a generation hands the new
//! connections of each endpoint to its new listener; open connections keep
//! the listener they were accepted under.
//!
//! ```text
//!   Endpoints (daemon)        Listener (generation)
//!   :443/udp ---- new connections ----> routes, limits, log, TLS
//!       \
//!        connection ---> one task per request stream ---> Service
//! ```

use std::collections::HashMap;
use std::fmt;
use std::future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

use bytes::{Buf, Bytes};
use h3::error::{Code, ConnectionError, StreamError};
use h3::server::{RequestResolver, RequestStream};
use http::header::{self, HeaderName};
use http::{Request, Response, Version};
use http_body_util::BodyExt;
use hyper::body::Frame;
use quinn::crypto::rustls::QuicServerConfig;
use quinn::{Endpoint, EndpointConfig, Incoming, TokioRuntime};
use socket2::{Domain, Socket, Type};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::quic::Suite;
use tokio_rustls::rustls::{CipherSuite, ServerConfig};

use crate::error::Error;
use crate::http::body::{self, Body, BodyError};
use crate::http::response::BoxResponse;
use crate::http::service::Service;
use crate::server::ConnectionContext;
use crate::systemd;
use crate::tls;

/// Connection-specific fields, which HTTP/3 forbids (RFC 9114, section
/// 4.2), removed from responses.
const CONNECTION_FIELDS: [HeaderName; 5] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Request stream of an HTTP/3 connection, or one of its halves.
type Stream<S> = RequestStream<S, Bytes>;

/// What one generation serves over HTTP/3 on one address.
pub(crate) struct Listener {
    address: SocketAddr,
    /// QUIC and TLS settings of new connections.
    config: quinn::ServerConfig,
    /// Routes, limits and log, shared with the TCP listener.
    context: Arc<ConnectionContext>,
}

impl fmt::Debug for Listener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Listener")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Listener on `address`, the address of the TCP listener whose
    /// `context` it serves, with the certificates and client verifier of
    /// its TLS acceptor `tls`.
    pub(crate) fn new(
        address: SocketAddr,
        tls: &TlsAcceptor,
        context: Arc<ConnectionContext>,
    ) -> io::Result<Self> {
        let crypto =
            QuicServerConfig::with_initial(tls::quic_config(tls.config()), initial_suite()?)
                .map_err(io::Error::other)?;
        Ok(Self {
            address,
            config: quinn::ServerConfig::with_crypto(Arc::new(crypto)),
            context,
        })
    }
}

/// Suite that protects QUIC Initial packets. It is always AES-128-GCM (RFC
/// 9001, section 5.2), even when `tls_options` leave that suite out of the
/// handshake.
fn initial_suite() -> io::Result<Suite> {
    let provider = Arc::clone(ServerConfig::builder().crypto_provider());
    provider
        .cipher_suites
        .iter()
        .filter_map(|suite| suite.tls13())
        .find(|suite| suite.common.suite == CipherSuite::TLS13_AES_128_GCM_SHA256)
        .and_then(|suite| suite.quic_suite())
        .ok_or_else(|| io::Error::other("the TLS provider has no AES-128-GCM suite for QUIC"))
}

/// QUIC endpoints of the daemon, by listen address. They outlive
/// generations.
#[derive(Default)]
pub(crate) struct Endpoints {
    endpoints: Mutex<HashMap<SocketAddr, Entry>>,
}

/// One endpoint and the listener it hands new connections to.
struct Entry {
    endpoint: Endpoint,
    listener: watch::Sender<Arc<Listener>>,
}

impl Endpoints {
    /// Makes `listeners` the ones new connections are served by, binding
    /// the addresses that have no endpoint yet. Every new address is bound
    /// before anything changes, so on error the endpoints serve what they
    /// served before. Endpoints `listeners` has no address for keep their
    /// listener.
    pub(crate) fn install(&self, listeners: &[Arc<Listener>]) -> Result<(), Error> {
        let mut endpoints = self
            .endpoints
            .lock()
            .expect("nothing panics while holding the endpoints");

        let mut bound = Vec::new();
        for listener in listeners {
            if !endpoints.contains_key(&listener.address) {
                let endpoint = bind(listener).map_err(|err| {
                    let address = listener.address;
                    io::Error::new(err.kind(), format!("HTTP/3 on {address} (UDP): {err}"))
                })?;
                bound.push((listener, endpoint));
            }
        }

        for listener in listeners {
            if let Some(entry) = endpoints.get(&listener.address) {
                entry
                    .endpoint
                    .set_server_config(Some(listener.config.clone()));
                entry.listener.send_replace(Arc::clone(listener));
            }
        }
        for (listener, endpoint) in bound {
            let (sender, receiver) = watch::channel(Arc::clone(listener));
            tokio::task::spawn(accept(endpoint.clone(), receiver));
            let entry = Entry {
                endpoint,
                listener: sender,
            };
            endpoints.insert(listener.address, entry);
        }
        Ok(())
    }
}

/// QUIC endpoint on the address of `listener`, over the UDP socket systemd
/// passed for it or a new one. Runs on the calling runtime.
fn bind(listener: &Listener) -> io::Result<Endpoint> {
    let address = listener.address;
    let socket = match systemd::udp_socket(address)? {
        Some(socket) => socket,
        None => {
            let socket = Socket::new(Domain::for_address(address), Type::DGRAM, None)?;
            // Lets the next process of a hot binary upgrade bind the port
            // while this one still serves it.
            #[cfg(unix)]
            socket.set_reuse_port(true)?;
            if address.is_ipv6() {
                socket.set_only_v6(true)?;
            }
            socket.bind(&address.into())?;
            socket.into()
        }
    };
    socket.set_nonblocking(true)?;
    Endpoint::new(
        EndpointConfig::default(),
        Some(listener.config.clone()),
        socket,
        Arc::new(TokioRuntime),
    )
}

/// Accepts the connections of `endpoint`, each one served by the listener
/// current when it arrives.
async fn accept(endpoint: Endpoint, listener: watch::Receiver<Arc<Listener>>) {
    while let Some(incoming) = endpoint.accept().await {
        let listener = Arc::clone(&listener.borrow());
        tokio::task::spawn(serve_connection(incoming, listener));
    }
}

/// Serves the requests of one QUIC connection until it closes.
async fn serve_connection(incoming: Incoming, listener: Arc<Listener>) {
    let log_name = &listener.context.log.log_name;
    let peer = incoming.remote_address();
    let connection = match incoming.await {
        Ok(connection) => connection,
        Err(err) => {
            eprintln!("{log_name} => QUIC handshake with {peer} failed: {err}");
            return;
        }
    };
    let mut connection = match h3::server::builder()
        .build(h3_quinn::Connection::new(connection))
        .await
    {
        Ok(connection) => connection,
        Err(err) => {
            eprintln!("{log_name} => HTTP/3 connection from {peer}: {err}");
            return;
        }
    };
    let service = listener
        .context
        .http_service(peer, listener.address, None)
        .expect("HTTP/3 listeners belong to HTTP servers");

    // Dropped, and so aborted, when the connection is lost: no response
    // can reach the client any more.
    let mut requests = JoinSet::new();
    loop {
        while requests.try_join_next().is_some() {}
        match connection.accept().await {
            Ok(Some(resolver)) => {
                requests.spawn(serve_request(resolver, service.clone()));
            }
            // The client sent GOAWAY: answer what it already asked for.
            Ok(None) => break,
            Err(err) => {
                // Closes and timeouts from the client side are routine.
                if matches!(err, ConnectionError::Local { .. }) && !err.is_h3_no_error() {
                    eprintln!("{log_name} => HTTP/3 connection from {peer}: {err}");
                }
                return;
            }
        }
    }
    while requests.join_next().await.is_some() {}
}

/// Answers the request of one stream with `service`.
async fn serve_request(resolver: RequestResolver<h3_quinn::Connection, Bytes>, service: Service) {
    // A stream reset before its head is complete has nothing to answer.
    let Ok((request, stream)) = resolver.resolve_request().await else {
        return;
    };
    let (mut send, receive) = stream.split();
    let (mut parts, ()) = request.into_parts();
    parts.version = Version::HTTP_3;
    let body = request_body(receive).await;
    let response = service.respond(Request::from_parts(parts, body)).await;
    // An error means the client reset the stream: it wants no answer.
    let _ = send_response(&mut send, response).await;
}

/// Reads a request body from its stream: data frames, then trailers.
struct BodyReader {
    stream: Stream<h3_quinn::RecvStream>,
    /// Set once the data frames are over.
    trailers: bool,
    done: bool,
}

impl BodyReader {
    fn poll_frame(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        if self.done {
            return Poll::Ready(None);
        }
        if !self.trailers {
            match ready!(self.stream.poll_recv_data(context)) {
                Ok(Some(mut data)) => {
                    let data = data.copy_to_bytes(data.remaining());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Ok(None) => self.trailers = true,
                Err(err) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(body_error(err))));
                }
            }
        }
        let trailers = ready!(self.stream.poll_recv_trailers(context));
        self.done = true;
        Poll::Ready(
            trailers
                .map_err(body_error)
                .transpose()
                .map(|trailers| trailers.map(Frame::trailers)),
        )
    }
}

fn body_error(err: StreamError) -> BodyError {
    BodyError::Io(io::Error::other(err))
}

/// Body of the request whose stream `stream` is the receiving half of. A
/// request that already ended, as most `GET`s do, gets an empty body, so it
/// goes upstream without one. Any other body is read by a task and handed
/// over through a channel: the stream itself is not `Sync`.
async fn request_body(stream: Stream<h3_quinn::RecvStream>) -> Body {
    let mut reader = BodyReader {
        stream,
        trailers: false,
        done: false,
    };
    let first = future::poll_fn(|context| Poll::Ready(reader.poll_frame(context))).await;
    if let Poll::Ready(None) = first {
        return body::empty();
    }

    let (sender, receiver) = mpsc::channel(1);
    tokio::task::spawn(async move {
        if let Poll::Ready(Some(frame)) = first
            && sender.send(frame).await.is_err()
        {
            return;
        }
        loop {
            let frame = tokio::select! {
                frame = future::poll_fn(|context| reader.poll_frame(context)) => frame,
                // The body was dropped: the rest of the stream is not needed.
                () = sender.closed() => return,
            };
            let Some(frame) = frame else {
                return;
            };
            if sender.send(frame).await.is_err() {
                return;
            }
        }
    });
    Received(receiver).boxed()
}

/// Request body fed by the task that reads its stream.
struct Received(mpsc::Receiver<Result<Frame<Bytes>, BodyError>>);

impl hyper::body::Body for Received {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        self.0.poll_recv(context)
    }
}

/// Sends `response` on `stream`: head, data, trailers, end. A body that
/// fails resets the stream with `H3_INTERNAL_ERROR`, so the client cannot
/// take a cut body for a whole one.
async fn send_response(
    stream: &mut Stream<h3_quinn::SendStream<Bytes>>,
    response: BoxResponse,
) -> Result<(), StreamError> {
    let (mut parts, mut body) = response.into_parts();
    for name in &CONNECTION_FIELDS {
        parts.headers.remove(name);
    }
    stream
        .send_response(Response::from_parts(parts, ()))
        .await?;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            stream.stop_stream(Code::H3_INTERNAL_ERROR);
            return Ok(());
        };
        match frame.into_data() {
            Ok(data) => stream.send_data(data).await?,
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    stream.send_trailers(trailers).await?;
                }
            }
        }
    }
    stream.finish().await
}
