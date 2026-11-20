//! HTTP/3 client over one QUIC connection, for tests of the HTTP/3 listener.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use h3::client::{RequestStream, SendRequest};
use h3::error::Code;
use http::{HeaderMap, Request, StatusCode};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::rustls::{ClientConfig, RootCertStore};
use quinn::{Connection, Endpoint, TransportConfig, VarInt};
use tokio::task::JoinHandle;

/// One QUIC connection to an HTTP/3 server, with its request sender.
pub struct Http3Client {
    // Kept so the client socket lives as long as the client.
    _endpoint: Endpoint,
    connection: Connection,
    sender: SendRequest<h3_quinn::OpenStreams, Bytes>,
    driver: JoinHandle<()>,
}

/// A request whose head was sent and whose stream is still open.
pub struct OpenRequest {
    stream: RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
}

impl OpenRequest {
    /// Waits for the response head, without ending the request, and
    /// returns its status.
    pub async fn status(&mut self) -> io::Result<StatusCode> {
        let response = self
            .stream
            .recv_response()
            .await
            .map_err(io::Error::other)?;
        Ok(response.status())
    }

    /// Resets the stream in both directions, as a client that gives up on
    /// the request does.
    pub fn reset(mut self) {
        self.stream.stop_stream(Code::H3_REQUEST_CANCELLED);
        self.stream.stop_sending(Code::H3_REQUEST_CANCELLED);
    }
}

/// A QUIC connection with ALPN `h3` that sends nothing and grants the
/// server no unidirectional stream, so the server cannot even open its
/// HTTP/3 control stream.
pub struct SilentConnection {
    // Kept so the client socket lives as long as the connection.
    _endpoint: Endpoint,
    connection: Connection,
}

impl SilentConnection {
    /// Resolves when the server closes the connection.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }
}

/// A complete HTTP/3 response.
#[derive(Debug)]
pub struct Http3Response {
    /// Status code.
    pub status: StatusCode,
    /// Header fields of the head.
    pub headers: HeaderMap,
    /// The whole body.
    pub body: Bytes,
    /// Trailer fields, if the server sent any.
    pub trailers: Option<HeaderMap>,
}

impl Http3Client {
    /// Connects to `address`, checking its certificate for `server_name`
    /// against the certificates of `root_pem`. Presents `identity` (the
    /// certificate chain PEM and the private key PEM) if given.
    pub async fn connect(
        address: SocketAddr,
        server_name: &str,
        root_pem: &str,
        identity: Option<(&str, &str)>,
    ) -> io::Result<Self> {
        let transport = TransportConfig::default();
        Self::connect_with(address, server_name, root_pem, identity, transport).await
    }

    /// [`Http3Client::connect`] without a client certificate, sending a
    /// PING every `keep_alive` while idle.
    pub async fn connect_keep_alive(
        address: SocketAddr,
        server_name: &str,
        root_pem: &str,
        keep_alive: Duration,
    ) -> io::Result<Self> {
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(keep_alive));
        Self::connect_with(address, server_name, root_pem, None, transport).await
    }

    async fn connect_with(
        address: SocketAddr,
        server_name: &str,
        root_pem: &str,
        identity: Option<(&str, &str)>,
        transport: TransportConfig,
    ) -> io::Result<Self> {
        let (endpoint, connection) =
            quic_connect(address, server_name, root_pem, identity, transport).await?;
        let (mut driver, sender) = h3::client::new(h3_quinn::Connection::new(connection.clone()))
            .await
            .map_err(io::Error::other)?;
        let driver = tokio::task::spawn(async move {
            driver.wait_idle().await;
        });
        Ok(Self {
            _endpoint: endpoint,
            connection,
            sender,
            driver,
        })
    }

    /// Completes a QUIC handshake with `address` as [`Http3Client::connect`]
    /// does, then leaves the connection silent: see [`SilentConnection`].
    pub async fn connect_silent(
        address: SocketAddr,
        server_name: &str,
        root_pem: &str,
    ) -> io::Result<SilentConnection> {
        let mut transport = TransportConfig::default();
        transport.max_concurrent_uni_streams(VarInt::from_u32(0));
        let (endpoint, connection) =
            quic_connect(address, server_name, root_pem, None, transport).await?;
        Ok(SilentConnection {
            _endpoint: endpoint,
            connection,
        })
    }

    /// Sends `request` with its body and reads the whole response. A stream
    /// the server resets is an error, even after a part of the body.
    pub async fn send(&mut self, request: Request<Bytes>) -> io::Result<Http3Response> {
        let (parts, body) = request.into_parts();
        let mut stream = self
            .sender
            .send_request(Request::from_parts(parts, ()))
            .await
            .map_err(io::Error::other)?;
        // A server may answer before it reads the whole body, then stop it
        // (RFC 9114, section 4.1): the response decides, not this error.
        let _ = async {
            if !body.is_empty() {
                stream.send_data(body).await?;
            }
            stream.finish().await
        }
        .await;

        let response = stream.recv_response().await.map_err(io::Error::other)?;
        let mut body = BytesMut::new();
        while let Some(chunk) = stream.recv_data().await.map_err(io::Error::other)? {
            body.put(chunk);
        }
        let trailers = stream.recv_trailers().await.map_err(io::Error::other)?;
        let (parts, ()) = response.into_parts();
        Ok(Http3Response {
            status: parts.status,
            headers: parts.headers,
            body: body.freeze(),
            trailers,
        })
    }

    /// Sends the head of `request` and leaves its stream open: no body, no
    /// end of stream.
    pub async fn open(&mut self, request: Request<()>) -> io::Result<OpenRequest> {
        let stream = self
            .sender
            .send_request(request)
            .await
            .map_err(io::Error::other)?;
        Ok(OpenRequest { stream })
    }

    /// Sends the head of `request` and the `partial` body bytes, then resets
    /// the stream in both directions without reading the response.
    pub async fn send_and_reset(&mut self, request: Request<()>, partial: &[u8]) -> io::Result<()> {
        let mut request = self.open(request).await?;
        request
            .stream
            .send_data(Bytes::copy_from_slice(partial))
            .await
            .map_err(io::Error::other)?;
        request.reset();
        Ok(())
    }

    /// Sends `frames`, already encoded HTTP/3 frames, as a whole request
    /// stream and returns every byte of the response stream. For requests
    /// the h3 client refuses to build, such as one over the server's header
    /// size limit.
    pub async fn send_raw(&self, frames: &[u8]) -> io::Result<Vec<u8>> {
        let (mut send, mut receive) = self.connection.open_bi().await?;
        send.write_all(frames).await?;
        send.finish()?;
        receive.read_to_end(1 << 20).await.map_err(io::Error::other)
    }

    /// Resolves when the server closes the connection.
    pub async fn closed(&self) {
        self.connection.closed().await;
    }
}

impl Drop for Http3Client {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

/// Completes a QUIC handshake with `address` under `transport`, checking
/// its certificate for `server_name` against the certificates of
/// `root_pem` and presenting `identity` if given.
async fn quic_connect(
    address: SocketAddr,
    server_name: &str,
    root_pem: &str,
    identity: Option<(&str, &str)>,
    transport: TransportConfig,
) -> io::Result<(Endpoint, Connection)> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(root_pem.as_bytes()) {
        roots
            .add(cert.map_err(io::Error::other)?)
            .map_err(io::Error::other)?;
    }
    let builder = ClientConfig::builder().with_root_certificates(roots);
    let mut tls = match identity {
        Some((chain, key)) => {
            let chain: Result<Vec<CertificateDer<'static>>, _> =
                CertificateDer::pem_slice_iter(chain.as_bytes()).collect();
            let chain = chain.map_err(io::Error::other)?;
            let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(io::Error::other)?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(io::Error::other)?
        }
        None => builder.with_no_client_auth(),
    };
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let quic = QuicClientConfig::try_from(tls).map_err(io::Error::other)?;

    let local = match address {
        SocketAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        SocketAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    };
    let mut endpoint = Endpoint::client(local)?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(Arc::new(transport));
    endpoint.set_default_client_config(config);
    let connection = endpoint
        .connect(address, server_name)
        .map_err(io::Error::other)?
        .await?;

    Ok((endpoint, connection))
}
