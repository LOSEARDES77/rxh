//! Integration tests for newly implemented feature set.

#![feature(trait_alias)]

mod util;

use std::{process::Stdio, sync::atomic::Ordering, time::Duration};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{
    Request,
    Response,
    header::{self},
    service::service_fn,
};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rcgen::generate_simple_self_signed;
use rxh::config::{
    Action,
    Algorithm,
    Backend,
    Cache,
    Forward,
    ForwardedHeader,
    Pattern,
    RequestConfig,
    RequestHeaders,
    ResponseConfig,
    ResponseHeaders,
    Server,
    ServerHeader,
    Tls,
    ViaHeader,
};
use tokio::{
    fs,
    net::TcpStream,
    process::Command,
    sync::mpsc,
    time::{sleep, timeout},
};
use tokio_rustls::{
    TlsConnector,
    rustls::{self, ClientConfig, RootCertStore},
};

use crate::util::{
    config,
    http::{send_http_request, spawn_backend_server, spawn_backend_server_with_request_counter},
    service::RequestInterceptor,
    tcp::{usable_socket, usable_tcp_listener},
};

#[tokio::test]
async fn request_and_response_header_customization_and_via() {
    let (listener, backend_addr) = usable_tcp_listener();
    let (tx, mut rx) = mpsc::channel(1);

    tokio::task::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        crate::util::service::serve_connection(stream, RequestInterceptor::new(tx)).await;
    });

    let backends = vec![Backend {
        address: backend_addr,
        weight: 1,
    }];

    let forward = Forward {
        algorithm: Algorithm::Wrr,
        backends: backends.clone(),
        scheduler: rxh::sched::make(Algorithm::Wrr, &backends),
    };

    let config = Server {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        patterns: vec![Pattern {
            uri: "/".into(),
            action: Action::Forward(forward),
            request: RequestConfig {
                headers: RequestHeaders {
                    forwarded: ForwardedHeader {
                        extend: true,
                        by: Some("edge-proxy".into()),
                    },
                    set: [("x-added".into(), "ok".into())].into_iter().collect(),
                    remove: vec!["x-remove-me".into()],
                },
            },
            response: ResponseConfig {
                headers: ResponseHeaders {
                    via: ViaHeader { extend: true },
                    server: ServerHeader {
                        should_override: true,
                        version: false,
                    },
                    set: [("x-response-added".into(), "yes".into())]
                        .into_iter()
                        .collect(),
                    remove: vec!["x-response-remove".into()],
                },
            },
            cache: Cache::default(),
        }],
        max_connections: 1024,
        name: Some("edge-proxy".into()),
        tls: None,
        http2: true,
        log_name: "unnamed".into(),
    };

    let server = rxh::Server::init(config, 0).unwrap();
    let proxy_addr = server.socket_address();
    tokio::task::spawn(async move { server.run().await.unwrap() });

    wait_until_listening(proxy_addr).await;

    let req = Request::builder()
        .header("x-remove-me", "delete")
        .header("x-response-remove", "request-side")
        .body(Empty::<Bytes>::new())
        .unwrap();

    let (parts, _) = send_http_request(proxy_addr, req).await;
    let (backend_parts, _) = rx.recv().await.unwrap();

    assert!(backend_parts.headers.contains_key("x-added"));
    assert!(!backend_parts.headers.contains_key("x-remove-me"));

    let forwarded = backend_parts
        .headers
        .get(header::FORWARDED)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(forwarded.contains("by=edge-proxy"));

    assert_eq!(
        parts.headers.get(header::SERVER).unwrap().to_str().unwrap(),
        "rxh"
    );
    assert!(parts.headers.contains_key(header::VIA));
    assert_eq!(
        parts
            .headers
            .get("x-response-added")
            .unwrap()
            .to_str()
            .unwrap(),
        "yes"
    );
    assert!(!parts.headers.contains_key("x-response-remove"));
}

#[tokio::test]
async fn cache_returns_second_response_without_hitting_backend() {
    let (backend, counter) = spawn_backend_server_with_request_counter(1);

    let backends = vec![backend.clone()];

    let forward = Forward {
        algorithm: Algorithm::Wrr,
        backends: backends.clone(),
        scheduler: rxh::sched::make(Algorithm::Wrr, &backends),
    };

    let config = Server {
        listen: vec!["127.0.0.1:0".parse().unwrap()],
        patterns: vec![Pattern {
            uri: "/".into(),
            action: Action::Forward(forward),
            request: RequestConfig::default(),
            response: ResponseConfig::default(),
            cache: Cache {
                enabled: true,
                ttl_seconds: 60,
                max_entries: 16,
            },
        }],
        max_connections: 1024,
        name: None,
        tls: None,
        http2: true,
        log_name: "unnamed".into(),
    };

    let server = rxh::Server::init(config, 0).unwrap();
    let proxy_addr = server.socket_address();
    tokio::task::spawn(async move { server.run().await.unwrap() });

    wait_until_listening(proxy_addr).await;

    let _ = send_http_request(proxy_addr, Request::new(Empty::<Bytes>::new())).await;
    let _ = send_http_request(proxy_addr, Request::new(Empty::<Bytes>::new())).await;

    assert_eq!(counter.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn http2_client_request_is_served() {
    let dir = util::temp_test_dir();
    fs::write(dir.path().join("index.txt"), b"h2 static")
        .await
        .unwrap();

    let (proxy_addr, _) = crate::util::http::spawn_reverse_proxy(config::files::serve(
        dir.path().to_str().unwrap(),
    ));
    wait_until_listening(proxy_addr).await;

    let stream = TcpStream::connect(proxy_addr).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http2::Builder::new(TokioExecutor::new())
        .handshake(TokioIo::new(stream))
        .await
        .unwrap();

    tokio::task::spawn(async move {
        conn.await.unwrap();
    });

    let req = Request::builder()
        .uri("/index.txt")
        .body(Empty::<Bytes>::new())
        .unwrap();

    let response = sender.send_request(req).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"h2 static"));
}

#[tokio::test]
async fn tls_listener_accepts_https_connections() {
    let cert = generate_simple_self_signed(vec!["localhost".into()]).unwrap();

    let dir = util::temp_test_dir();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");

    fs::write(&cert_path, cert.cert.pem()).await.unwrap();
    fs::write(&key_path, cert.signing_key.serialize_pem())
        .await
        .unwrap();

    let (backend_addr, _) = spawn_backend_server(service_fn(|_| async {
        Ok::<_, std::convert::Infallible>(Response::new(Full::<Bytes>::from("tls-ok")))
    }));

    let mut cfg = crate::util::config::proxy::single_backend(backend_addr);
    cfg.tls = Some(Tls {
        cert: cert_path.to_string_lossy().to_string(),
        key: key_path.to_string_lossy().to_string(),
    });

    let server = rxh::Server::init(cfg, 0).unwrap();
    let proxy_addr = server.socket_address();
    tokio::task::spawn(async move { server.run().await.unwrap() });

    wait_until_listening(proxy_addr).await;

    let mut roots = RootCertStore::empty();
    let cert_der = rustls_pemfile::certs(&mut cert.cert.pem().as_bytes())
        .next()
        .unwrap()
        .unwrap();
    roots.add(cert_der).unwrap();

    let tls = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();

    let connector = TlsConnector::from(std::sync::Arc::new(tls));
    let stream = TcpStream::connect(proxy_addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .unwrap()
        .to_owned();
    let tls_stream = connector.connect(server_name, stream).await.unwrap();

    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tls_stream))
        .await
        .unwrap();
    tokio::task::spawn(async move {
        conn.with_upgrades().await.unwrap();
    });

    let response = sender
        .send_request(Request::new(Empty::<Bytes>::new()))
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(body, Bytes::from_static(b"tls-ok"));
}

#[cfg(unix)]
#[tokio::test]
async fn sighup_reloads_configuration_in_binary() {
    let (backend1_addr, _) = spawn_backend_server(service_fn(|_| async {
        Ok::<_, std::convert::Infallible>(Response::new(Full::<Bytes>::from("backend-1")))
    }));
    let (backend2_addr, _) = spawn_backend_server(service_fn(|_| async {
        Ok::<_, std::convert::Infallible>(Response::new(Full::<Bytes>::from("backend-2")))
    }));

    let (_, proxy_addr) = usable_socket();
    let dir = util::temp_test_dir();
    let config_path = dir.path().join("rxh.toml");

    fs::write(
        &config_path,
        format!("[[server]]\nlisten = \"{proxy_addr}\"\nforward = \"{backend1_addr}\"\n"),
    )
    .await
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rxh"))
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    wait_until_listening(proxy_addr).await;

    let body1 = get_body_with_retry(proxy_addr, Duration::from_secs(3)).await;
    assert_eq!(body1, Bytes::from_static(b"backend-1"));

    fs::write(
        &config_path,
        format!("[[server]]\nlisten = \"{proxy_addr}\"\nforward = \"{backend2_addr}\"\n"),
    )
    .await
    .unwrap();

    let pid = child.id().unwrap();
    let status = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());

    let switched = timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(body) = try_get_body(proxy_addr).await
                && body == Bytes::from_static(b"backend-2")
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    assert!(switched.is_ok());

    let _ = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await;

    let _ = timeout(Duration::from_secs(3), child.wait()).await;
}

#[cfg(unix)]
#[tokio::test]
async fn cli_reload_and_kill_daemon_options_control_running_binary() {
    let (backend1_addr, _) = spawn_backend_server(service_fn(|_| async {
        Ok::<_, std::convert::Infallible>(Response::new(Full::<Bytes>::from("backend-1")))
    }));
    let (backend2_addr, _) = spawn_backend_server(service_fn(|_| async {
        Ok::<_, std::convert::Infallible>(Response::new(Full::<Bytes>::from("backend-2")))
    }));

    let (_, proxy_addr) = usable_socket();
    let dir = util::temp_test_dir();
    let config_path = dir.path().join("rxh.toml");
    let pid_file_name = "rxh.pid";
    let pid_file = dir.path().join(pid_file_name);

    fs::write(
        &config_path,
        format!("[[server]]\nlisten = \"{proxy_addr}\"\nforward = \"{backend1_addr}\"\n"),
    )
    .await
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_rxh"))
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let pid = child.id().unwrap();
    fs::write(&pid_file, format!("{pid}\n")).await.unwrap();

    wait_until_listening(proxy_addr).await;

    let body1 = get_body_with_retry(proxy_addr, Duration::from_secs(3)).await;
    assert_eq!(body1, Bytes::from_static(b"backend-1"));

    fs::write(
        &config_path,
        format!("[[server]]\nlisten = \"{proxy_addr}\"\nforward = \"{backend2_addr}\"\n"),
    )
    .await
    .unwrap();

    let reload_status = Command::new(env!("CARGO_BIN_EXE_rxh"))
        .current_dir(&dir)
        .args(["--reload-config", "--pid-file", pid_file_name])
        .status()
        .await
        .unwrap();
    assert!(reload_status.success());

    let switched = timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(body) = try_get_body(proxy_addr).await
                && body == Bytes::from_static(b"backend-2")
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(switched.is_ok());

    let kill_status = Command::new(env!("CARGO_BIN_EXE_rxh"))
        .current_dir(&dir)
        .args(["--kill-daemon", "--pid-file", pid_file_name])
        .status()
        .await
        .unwrap();
    assert!(kill_status.success());

    let stopped = timeout(Duration::from_secs(3), child.wait()).await;
    assert!(stopped.is_ok(), "process did not stop after --kill-daemon");
}

async fn wait_until_listening(addr: std::net::SocketAddr) {
    let started = timeout(Duration::from_secs(5), async {
        loop {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await;

    assert!(started.is_ok(), "server did not start listening in time");
}

async fn get_body_with_retry(addr: std::net::SocketAddr, max_wait: Duration) -> Bytes {
    timeout(max_wait, async {
        loop {
            if let Ok(body) = try_get_body(addr).await {
                return body;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("request retry timed out")
}

async fn try_get_body(addr: std::net::SocketAddr) -> Result<Bytes, ()> {
    let stream = TcpStream::connect(addr).await.map_err(|_| ())?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| ())?;

    tokio::task::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let response = sender
        .send_request(Request::new(Empty::<Bytes>::new()))
        .await
        .map_err(|_| ())?;

    response
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .map_err(|_| ())
}
