//! HTTPS and unix socket backends.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

async fn answer_with_host(request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let host = request
        .headers()
        .get(header::HOST)
        .map_or("", |value| value.to_str().unwrap_or(""))
        .to_owned();
    Ok(Response::new(Full::from(host)))
}

/// HTTPS backend with a fresh certificate for `localhost`, written to `dir/ca.pem`.
/// It answers with the `Host` header it received.
async fn https_backend(dir: &Path) -> SocketAddr {
    let certificate = rcgen::generate_simple_self_signed(vec![String::from("localhost")]).unwrap();
    std::fs::write(dir.join("ca.pem"), certificate.cert.pem()).unwrap();

    let chain = vec![CertificateDer::from(certificate.cert.der().to_vec())];
    let key = PrivateKeyDer::try_from(certificate.signing_key.serialize_der()).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::task::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            tokio::task::spawn(async move {
                let Ok(stream) = acceptor.accept(stream).await else {
                    return;
                };
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service_fn(answer_with_host))
                    .await;
            });
        }
    });
    address
}

fn proxy_config(backend: &str, tls: &str) -> String {
    format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {{ backends = [\"{backend}\"], \
         tls = {{ {tls} }} }}"
    )
}

#[tokio::test]
async fn https_backend_trusted_through_ca() {
    let dir = tempfile::tempdir().unwrap();
    let backend = https_backend(dir.path()).await;
    let ca = dir.path().join("ca.pem");
    let proxy = util::spawn(&proxy_config(
        &format!("https://{backend}"),
        &format!(
            "ca = {:?}, server_name = \"localhost\"",
            ca.to_str().unwrap()
        ),
    ))[0];

    let (status, _, body) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::OK);
    // Like Caddy, the backend sees its own address as `Host`.
    assert_eq!(body, backend.to_string());
}

#[tokio::test]
async fn https_backend_with_insecure_skip_verify() {
    let dir = tempfile::tempdir().unwrap();
    let backend = https_backend(dir.path()).await;
    let proxy = util::spawn(&proxy_config(
        &format!("https://{backend}"),
        "insecure_skip_verify = true",
    ))[0];

    assert_eq!(util::get(proxy, "/").await.0, StatusCode::OK);
}

#[tokio::test]
async fn https_backend_untrusted_is_bad_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let backend = https_backend(dir.path()).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"https://{backend}\""
    ))[0];

    assert_eq!(util::get(proxy, "/").await.0, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn https_backend_keeps_user_host_header() {
    let dir = tempfile::tempdir().unwrap();
    let backend = https_backend(dir.path()).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nrequest = {{ headers = {{ set = {{ Host = \"example.test\" }} }} }}\n\
         forward = {{ backends = [\"https://{backend}\"], tls = {{ insecure_skip_verify = true }} }}"
    ))[0];

    assert_eq!(util::get(proxy, "/").await.2, "example.test");
}

#[tokio::test]
async fn https_active_health_check_uses_tls() {
    let dir = tempfile::tempdir().unwrap();
    let backend = https_backend(dir.path()).await;
    let (sockets, registry) = util::spawn_with_registry(&format!(
        "[upstream.secure]\nbackends = [\"https://{backend}\"]\n\
         tls = {{ insecure_skip_verify = true }}\n\
         health = {{ uri = \"/\", interval = \"50ms\", timeout = \"1s\" }}\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@secure\""
    ));
    let _checks = registry.start_background_tasks();

    let group = registry.get("secure").unwrap();
    util::eventually(async || {
        group
            .pool
            .upstreams()
            .iter()
            .all(|upstream| upstream.is_healthy())
    })
    .await;
    assert_eq!(util::get(sockets[0], "/").await.0, StatusCode::OK);
}

#[cfg(unix)]
mod unix {
    use tokio::net::UnixListener;

    use super::*;

    #[tokio::test]
    async fn http_backend_on_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::task::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                tokio::task::spawn(async move {
                    let service = service_fn(|_| async {
                        Ok::<_, Infallible>(Response::new(Full::<Bytes>::from("over-unix")))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });

        let proxy = util::spawn(&format!(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"unix/{}\"",
            path.to_str().unwrap()
        ))[0];

        let (status, _, body) = util::get(proxy, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "over-unix");
    }

    #[tokio::test]
    async fn tcp_echo_backend_on_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("echo.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::task::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::task::spawn(async move {
                    let mut buffer = [0; 64];
                    while let Ok(read @ 1..) = stream.read(&mut buffer).await {
                        if stream.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });

        let proxy = util::spawn(&format!(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"tcp\"\nforward = \"unix/{}\"",
            path.to_str().unwrap()
        ))[0];

        let mut stream = tokio::net::TcpStream::connect(proxy).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ping");
    }
}

#[test]
fn validation_errors() {
    let error = |text: &str| Config::from_toml(text).unwrap_err().to_string();
    let server =
        |forward: &str| format!("[[server]]\nlisten = \"127.0.0.1:0\"\nforward = {forward}\n");

    assert!(
        error(&server("{ backends = [\"https://a\", \"b:80\"] }"))
            .contains("schemes can't be mixed")
    );
    assert!(
        error(&server(
            "{ backends = [\"https://a\", \"unix//run/a.sock\"] }"
        ))
        .contains("schemes can't be mixed")
    );
    assert!(error(&server("\"unix/relative\"")).contains("invalid backend address"));
    assert!(error(&server("\"a\"")).contains("invalid backend address"));
    assert!(
        error(&server(
            "{ backends = [\"a:1\"], tls = { client_cert = \"c.pem\" } }"
        ))
        .contains("client_cert and tls.client_key")
    );
    assert!(
        error(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"udp\"\nforward = \"unix//run/a.sock\"\n"
        )
        .contains("not supported for udp")
    );
    assert!(
        Config::from_toml(&server(
            "{ backends = [\"https://a:8001-8002\", \"https://b\"] }"
        ))
        .is_ok()
    );
}

#[tokio::test]
async fn runtime_add_enforces_the_same_rules() {
    use rxh_server::config::Backend;
    use rxh_server::pool::PoolError;

    let registry = util::spawn_with_registry(
        "[upstream.plain]\nbackends = [\"a:1\"]\n\
         [upstream.secure]\nbackends = [\"https://b\"]\n\
         [upstream.forced]\nbackends = [\"c:1\"]\ntls = { insecure_skip_verify = true }\n\
         [upstream.dgram]\nbackends = [\"d:1\"]\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@plain\"\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@secure\"\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@forced\"\n\
         [[server]]\nlisten = \"127.0.0.1:0\"\nprotocol = \"udp\"\nforward = \"@dgram\"\n",
    )
    .1;
    let add =
        |pool: &str, address: &str| registry.get(pool).unwrap().pool.add(Backend::new(address));

    let Err(PoolError::InvalidAddress(message)) = add("plain", "https://x") else {
        panic!("https into a plain pool must fail");
    };
    assert!(message.contains("schemes can't be mixed"), "{message}");
    assert!(add("secure", "x:1").is_err());
    assert!(add("plain", "unix//run/x.sock").is_ok());
    assert!(add("plain", "y:2").is_ok());
    assert!(add("secure", "https://z").is_ok());
    assert!(add("forced", "https://z").is_ok());

    let Err(PoolError::InvalidAddress(message)) = add("dgram", "unix//run/x.sock") else {
        panic!("unix into a udp pool must fail");
    };
    assert!(message.contains("udp"), "{message}");
    assert!(add("dgram", "e:1").is_ok());
}
