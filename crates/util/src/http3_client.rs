//! HTTP/3 client over one QUIC connection, for tests of the HTTP/3 listener.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use bytes::{BufMut, Bytes, BytesMut};
use h3::client::SendRequest;
use h3::error::Code;
use http::{HeaderMap, Request, StatusCode};
use quinn::crypto::rustls::QuicClientConfig;
use quinn::rustls::pki_types::pem::PemObject;
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use quinn::rustls::{ClientConfig, RootCertStore};
use quinn::{Connection, Endpoint};
use tokio::task::JoinHandle;

/// One QUIC connection to an HTTP/3 server, with its request sender.
pub struct Http3Client {
    // Kept so the client socket lives as long as the client.
    _endpoint: Endpoint,
    connection: Connection,
    sender: SendRequest<h3_quinn::OpenStreams, Bytes>,
    driver: JoinHandle<()>,
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
                let key =
                    PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(io::Error::other)?;
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
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic)));
        let connection = endpoint
            .connect(address, server_name)
            .map_err(io::Error::other)?
            .await?;

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

    /// Sends the head of `request` and the `partial` body bytes, then resets
    /// the stream in both directions without reading the response.
    pub async fn send_and_reset(&mut self, request: Request<()>, partial: &[u8]) -> io::Result<()> {
        let mut stream = self
            .sender
            .send_request(request)
            .await
            .map_err(io::Error::other)?;
        stream
            .send_data(Bytes::copy_from_slice(partial))
            .await
            .map_err(io::Error::other)?;
        stream.stop_stream(Code::H3_REQUEST_CANCELLED);
        stream.stop_sending(Code::H3_REQUEST_CANCELLED);
        Ok(())
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
