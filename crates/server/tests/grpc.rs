//! gRPC health checks and gRPC-Web translation, end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{HeaderMap, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};

/// `status` value that makes the backend answer `UNIMPLEMENTED`.
const UNIMPLEMENTED: u8 = 255;

/// Response body: one data frame, then trailers.
struct Reply {
    data: Option<Bytes>,
    trailers: Option<HeaderMap>,
}

impl hyper::body::Body for Reply {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if let Some(data) = self.data.take() {
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
        Poll::Ready(
            self.trailers
                .take()
                .map(|trailers| Ok(Frame::trailers(trailers))),
        )
    }
}

/// What the gRPC backend saw and how it answers health checks.
#[derive(Clone, Default)]
struct Grpc {
    /// `HealthCheckResponse.status` to answer, or [`UNIMPLEMENTED`].
    status: Arc<AtomicU8>,
    /// Body of the last health check.
    checked: Arc<Mutex<Option<Bytes>>>,
    /// Head of the last call that was not a health check.
    called: Arc<Mutex<Option<http::request::Parts>>>,
}

/// HTTP/2 only backend speaking enough gRPC: the health service, and an
/// echo of the request body for every other path.
async fn grpc_backend() -> (SocketAddr, Grpc) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let grpc = Grpc::default();
    grpc.status.store(1, Ordering::SeqCst);
    let state = grpc.clone();
    tokio::task::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            let service = service_fn(move |request: Request<Incoming>| {
                let state = state.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    let mut response = Response::new(Reply {
                        data: None,
                        trailers: None,
                    });
                    let content_type = parts.headers["content-type"].clone();
                    response.headers_mut().insert("content-type", content_type);
                    let mut trailers = HeaderMap::new();
                    trailers.insert("grpc-status", "0".parse().unwrap());
                    if parts.uri.path() == "/grpc.health.v1.Health/Check" {
                        *state.checked.lock().unwrap() = Some(body);
                        match state.status.load(Ordering::SeqCst) {
                            UNIMPLEMENTED => {
                                let headers = response.headers_mut();
                                headers.insert("grpc-status", "12".parse().unwrap());
                            }
                            status => {
                                let frame = vec![0, 0, 0, 0, 2, 8, status];
                                response.body_mut().data = Some(Bytes::from(frame));
                                response.body_mut().trailers = Some(trailers);
                            }
                        }
                    } else {
                        *state.called.lock().unwrap() = Some(parts);
                        trailers.insert("grpc-message", "done".parse().unwrap());
                        response.body_mut().data = Some(body);
                        response.body_mut().trailers = Some(trailers);
                    }
                    Ok::<_, Infallible>(response)
                }
            });
            tokio::task::spawn(async move {
                let _ = hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (address, grpc)
}

/// Proxy in front of `backend`, with `options` on its pool.
fn proxy(backend: SocketAddr, options: &str) -> (SocketAddr, Arc<rxh_server::Registry>) {
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.grpc]
        backends = ["{backend}"]
        {options}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@grpc"
        "#
    ));
    (proxies[0], registry)
}

/// `POST /echo.Echo/Say` over HTTP/1.1, as a browser without HTTP/2 would.
async fn call(
    proxy: SocketAddr,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, HeaderMap, Bytes) {
    let stream = TcpStream::connect(proxy).await.unwrap();
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::task::spawn(connection);
    let request = Request::post("/echo.Echo/Say")
        .header("host", "grpc.test")
        .header("content-type", content_type)
        .header("x-grpc-web", "1")
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        body.collect().await.unwrap().to_bytes(),
    )
}

/// gRPC frame of a seven byte message: twelve bytes, so its base64 has no
/// padding.
const MESSAGE: [u8; 12] = [0, 0, 0, 0, 7, b'p', b'a', b'y', b'l', b'o', b'a', b'd'];

/// The trailers of the echo backend as a gRPC-Web frame.
fn trailers_frame() -> Vec<u8> {
    let block = b"grpc-status: 0\r\ngrpc-message: done\r\n";
    let mut frame = vec![0x80, 0, 0, 0, block.len() as u8];
    frame.extend_from_slice(block);
    frame
}

#[tokio::test]
async fn grpc_health_check_follows_the_serving_status() {
    let (backend, grpc) = grpc_backend().await;
    let (proxy, registry) = proxy(
        backend,
        r#"h2c = true
        health = { grpc = true, grpc_service = "echo.Echo", interval = "50ms", timeout = "1s" }"#,
    );
    let _checks = registry.start_background_tasks();
    let pool = registry.get("grpc").unwrap();
    let upstream = &pool.pool.upstreams()[0];

    // The check names the service: field 1, length 9.
    util::eventually(async || grpc.checked.lock().unwrap().is_some()).await;
    let mut expected = vec![0, 0, 0, 0, 11, 0x0a, 9];
    expected.extend_from_slice(b"echo.Echo");
    assert_eq!(grpc.checked.lock().unwrap().as_deref(), Some(&expected[..]));
    assert!(upstream.is_healthy());

    // NOT_SERVING takes the backend out of rotation.
    grpc.status.store(2, Ordering::SeqCst);
    util::eventually(async || !upstream.is_healthy()).await;
    let (status, _, _) = call(proxy, "application/grpc", MESSAGE.to_vec()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    grpc.status.store(1, Ordering::SeqCst);
    util::eventually(async || upstream.is_healthy()).await;

    // So does a server without the health service.
    grpc.status.store(UNIMPLEMENTED, Ordering::SeqCst);
    util::eventually(async || !upstream.is_healthy()).await;
}

#[tokio::test]
async fn grpc_health_check_fails_backends_that_do_not_speak_grpc() {
    let health = r#"health = { grpc = true, interval = "50ms", timeout = "500ms" }"#;

    // HTTP/1.1 only: the HTTP/2 preface gets no usable answer.
    let (_, registry) = proxy(util::backend("plain").await, health);
    let _checks = registry.start_background_tasks();
    let pool = registry.get("grpc").unwrap();
    util::eventually(async || !pool.pool.upstreams()[0].is_healthy()).await;

    let (_, registry) = proxy(util::dead_address(), health);
    let _dead_checks = registry.start_background_tasks();
    let pool = registry.get("grpc").unwrap();
    util::eventually(async || !pool.pool.upstreams()[0].is_healthy()).await;
}

#[tokio::test]
async fn grpc_web_binary_gets_trailers_in_the_body() {
    let (backend, grpc) = grpc_backend().await;
    let (proxy, _) = proxy(backend, "h2c = true\ngrpc_web = true");

    let (status, headers, body) = call(proxy, "application/grpc-web+proto", MESSAGE.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/grpc-web+proto");
    let mut expected = MESSAGE.to_vec();
    expected.extend_from_slice(&trailers_frame());
    assert_eq!(body[..], expected[..]);

    // The backend got plain gRPC.
    let called = grpc.called.lock().unwrap().take().unwrap();
    assert_eq!(called.headers["content-type"], "application/grpc+proto");
    assert_eq!(called.headers["te"], "trailers");
}

#[tokio::test]
async fn grpc_web_text_is_base64_both_ways() {
    let (backend, grpc) = grpc_backend().await;
    let (proxy, _) = proxy(backend, "h2c = true\ngrpc_web = true");

    let request = STANDARD.encode(MESSAGE).into_bytes();
    let (status, headers, body) = call(proxy, "application/grpc-web-text", request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/grpc-web-text");
    let mut expected = MESSAGE.to_vec();
    expected.extend_from_slice(&trailers_frame());
    assert_eq!(STANDARD.decode(&body).unwrap(), expected);

    let called = grpc.called.lock().unwrap().take().unwrap();
    assert_eq!(called.headers["content-type"], "application/grpc");

    // A body that is not base64 never becomes a gRPC call.
    let (status, _, _) = call(proxy, "application/grpc-web-text", b"!!!!".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(grpc.called.lock().unwrap().is_none());
}

#[tokio::test]
async fn only_grpc_web_requests_of_grpc_web_pools_are_translated() {
    let (backend, grpc) = grpc_backend().await;

    // Native gRPC through a `grpc_web` pool is untouched.
    let (translating, _) = proxy(backend, "h2c = true\ngrpc_web = true");
    let (status, headers, body) = call(translating, "application/grpc", MESSAGE.to_vec()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/grpc");
    assert_eq!(body[..], MESSAGE[..]);

    // Without the option gRPC-Web reaches the backend as it came.
    let (plain, _) = proxy(backend, "h2c = true");
    let (_, headers, body) = call(plain, "application/grpc-web", MESSAGE.to_vec()).await;
    assert_eq!(headers["content-type"], "application/grpc-web");
    assert_eq!(body[..], MESSAGE[..]);
    let called = grpc.called.lock().unwrap().take().unwrap();
    assert_eq!(called.headers["content-type"], "application/grpc-web");
}
