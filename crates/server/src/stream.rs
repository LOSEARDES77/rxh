//! Connections to backends: TCP, unix socket, or either wrapped in TLS, all
//! behind one [`UpstreamStream`] type produced by [`Dialer::dial`].

use std::fs;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rxh_core::address::BackendAddress;
use rxh_core::config::{UpstreamGroup, UpstreamTls};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{self, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
};

use crate::timeout::Timed;

/// Transport under TLS, or used as is.
#[derive(Debug)]
pub(crate) enum Raw {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

/// An open connection to a backend.
#[derive(Debug)]
pub(crate) enum UpstreamStream {
    /// Plain TCP.
    Tcp(TcpStream),

    /// Plain unix domain socket.
    #[cfg(unix)]
    Unix(UnixStream),

    /// TLS over TCP or a unix socket.
    Tls(Box<TlsStream<Raw>>),
}

macro_rules! delegate {
    ($self:ident, $stream:ident => $call:expr) => {
        match $self.get_mut() {
            Self::Tcp($stream) => $call,
            #[cfg(unix)]
            Self::Unix($stream) => $call,
        }
    };
}

impl AsyncRead for Raw {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for Raw {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, s => Pin::new(s).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_shutdown(cx))
    }
}

macro_rules! delegate_stream {
    ($self:ident, $stream:ident => $call:expr) => {
        match $self.get_mut() {
            Self::Tcp($stream) => $call,
            #[cfg(unix)]
            Self::Unix($stream) => $call,
            Self::Tls($stream) => $call,
        }
    };
}

impl AsyncRead for UpstreamStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate_stream!(self, s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for UpstreamStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate_stream!(self, s => Pin::new(s).poll_write(cx, buf))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate_stream!(self, s => Pin::new(s).poll_flush(cx))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate_stream!(self, s => Pin::new(s).poll_shutdown(cx))
    }
}

#[derive(Debug)]
struct Tls {
    config: Arc<ClientConfig>,
    server_name: Option<String>,
}

/// Opens connections to the backends of one pool.
#[derive(Debug)]
pub(crate) struct Dialer {
    timeout: Duration,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    tls: Option<Tls>,
}

impl Dialer {
    /// Dialer for `group`, loading its TLS files.
    pub(crate) fn new(group: &UpstreamGroup) -> io::Result<Self> {
        let tls = if group.uses_tls() {
            let settings = group.tls.clone().unwrap_or_default();
            Some(Tls {
                config: Arc::new(client_config(&settings, group.h2c)?),
                server_name: settings.server_name,
            })
        } else {
            None
        };
        Ok(Self {
            timeout: group.dial_timeout,
            read_timeout: group.read_timeout,
            write_timeout: group.write_timeout,
            tls,
        })
    }

    /// True when connections are wrapped in TLS.
    pub(crate) fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// Connects to `address` (any backend address form) within `dial_timeout`,
    /// including the TLS handshake when the pool uses TLS. The stream fails
    /// reads and writes that make no progress for `read_timeout` and
    /// `write_timeout`.
    pub(crate) async fn dial(&self, address: &str) -> io::Result<Timed<UpstreamStream>> {
        let stream = tokio::time::timeout(self.timeout, self.connect(address))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial timeout"))??;
        Ok(Timed::new(stream, self.read_timeout, self.write_timeout))
    }

    async fn connect(&self, address: &str) -> io::Result<UpstreamStream> {
        let parsed = BackendAddress::parse(address)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;

        let (raw, host) = match parsed {
            BackendAddress::Tcp { host, port, .. } => {
                let stream = TcpStream::connect((host.trim_matches(['[', ']']), port)).await?;
                stream.set_nodelay(true)?;
                (Raw::Tcp(stream), host)
            }
            #[cfg(unix)]
            BackendAddress::Unix(path) => {
                (Raw::Unix(UnixStream::connect(path).await?), "localhost")
            }
            #[cfg(not(unix))]
            BackendAddress::Unix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unix sockets are not supported on this platform",
                ));
            }
        };

        let Some(tls) = &self.tls else {
            return Ok(match raw {
                Raw::Tcp(stream) => UpstreamStream::Tcp(stream),
                #[cfg(unix)]
                Raw::Unix(stream) => UpstreamStream::Unix(stream),
            });
        };

        let name = tls
            .server_name
            .as_deref()
            .unwrap_or(host.trim_matches(['[', ']']));
        let name = ServerName::try_from(name.to_owned())
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        let stream = TlsConnector::from(Arc::clone(&tls.config))
            .connect(name, raw)
            .await?;
        Ok(UpstreamStream::Tls(Box::new(stream)))
    }
}

fn client_config(settings: &UpstreamTls, h2: bool) -> io::Result<ClientConfig> {
    let builder = ClientConfig::builder();
    let provider = Arc::clone(builder.crypto_provider());

    let builder = if settings.insecure_skip_verify {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerify(provider)))
    } else {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if let Some(path) = &settings.ca {
            for cert in read_certs(path)? {
                roots.add(cert).map_err(io::Error::other)?;
            }
        }
        builder.with_root_certificates(roots)
    };

    let mut config = match (&settings.client_cert, &settings.client_key) {
        (Some(cert), Some(key)) => {
            let key =
                fs::read(key).map_err(|err| io::Error::new(err.kind(), format!("{key}: {err}")))?;
            let key = rustls_pemfile::private_key(&mut key.as_slice())?
                .ok_or_else(|| io::Error::other(format!("{cert}: no private key found")))?;
            builder
                .with_client_auth_cert(read_certs(cert)?, key)
                .map_err(io::Error::other)?
        }
        _ => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![if h2 { &b"h2"[..] } else { &b"http/1.1"[..] }.to_vec()];
    Ok(config)
}

fn read_certs(path: &str) -> io::Result<Vec<CertificateDer<'static>>> {
    let pem = fs::read(path).map_err(|err| io::Error::new(err.kind(), format!("{path}: {err}")))?;
    let certs = rustls_pemfile::certs(&mut pem.as_slice()).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err(io::Error::other(format!("{path}: no certificate found")));
    }
    Ok(certs)
}

/// Accepts every certificate (`insecure_skip_verify`).
#[derive(Debug)]
struct SkipVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
