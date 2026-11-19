//! Drain and maintenance states set through the admin API, end to end.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use rxh_server::Daemon;
use rxh_test_util::free_address;
use serde_json::Value;
use tokio::net::TcpStream;

async fn admin_call(
    admin: SocketAddr,
    method: Method,
    uri: &str,
    body: &str,
) -> (StatusCode, Value) {
    let stream = TcpStream::connect(admin).await.expect("admin accepts");
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .expect("handshake");
    tokio::task::spawn(connection);
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, admin.to_string())
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("valid request");
    let response = sender.send_request(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body").to_bytes();
    (
        parts.status,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}

async fn set_state(admin: SocketAddr, address: SocketAddr, state: &str) -> (StatusCode, Value) {
    let body = format!(r#"{{"address": "{address}", "state": "{state}"}}"#);
    admin_call(admin, Method::PUT, "/pools/api/upstreams/state", &body).await
}

async fn bodies(web: SocketAddr, count: usize) -> Vec<String> {
    let mut bodies = Vec::with_capacity(count);
    for _ in 0..count {
        let (status, _, body) = util::get(web, "/").await;
        assert_eq!(status, StatusCode::OK);
        bodies.push(String::from_utf8_lossy(&body).into_owned());
    }
    bodies
}

#[tokio::test(flavor = "multi_thread")]
async fn drained_backend_finishes_requests_in_flight_and_gets_no_new_ones() {
    let slow = util::backend_with(|_| async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        Response::new(Full::new(Bytes::from_static(b"slow")))
    })
    .await;
    let fast = util::backend("fast").await;
    let (admin, web) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\n\
             [upstream.api]\npolicy = \"first\"\nbackends = [\"{slow}\", \"{fast}\"]\n\
             [[server]]\nlisten = \"{web}\"\nforward = \"@api\"\n"
        ),
    )
    .expect("write config");
    let _daemon = tokio::task::spawn(Daemon::new(&config_path).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    // A request in flight to the backend being drained still gets its answer.
    let in_flight = tokio::task::spawn(util::get(web, "/"));
    util::eventually(async || {
        let (_, pool) = admin_call(admin, Method::GET, "/pools/api", "").await;
        pool["upstreams"][0]["num_requests"] == 1
    })
    .await;
    let (status, pool) = set_state(admin, slow, "drain").await;
    assert_eq!(status, StatusCode::OK, "{pool}");
    assert_eq!(pool["upstreams"][0]["state"], "drain");
    assert_eq!(pool["upstreams"][0]["healthy"], true);
    assert_eq!(pool["upstreams"][0]["available"], false);
    assert_eq!(pool["upstreams"][1]["state"], "ready");
    assert_eq!(bodies(web, 5).await, ["fast"; 5]);
    let (status, _, body) = in_flight.await.expect("request task");
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"slow"[..]));

    // A reload keeps the state of the backends it keeps.
    let (status, json) = admin_call(admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (_, pool) = admin_call(admin, Method::GET, "/pools/api", "").await;
    assert_eq!(pool["upstreams"][0]["state"], "drain");
    assert_eq!(bodies(web, 3).await, ["fast"; 3]);

    // With every backend out of rotation the pool answers 503.
    let (status, _) = set_state(admin, fast, "maintenance").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = util::get(web, "/").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // Back to ready.
    let (status, _) = set_state(admin, slow, "ready").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bodies(web, 1).await, ["slow"]);

    // Unknown members, pools and states.
    let (status, json) = set_state(admin, free_address(), "drain").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("is not in pool api")
    );
    let body = format!(r#"{{"address": "{slow}", "state": "drain"}}"#);
    let (status, _) = admin_call(admin, Method::PUT, "/pools/nope/upstreams/state", &body).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, json) = set_state(admin, slow, "sleeping").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .starts_with("invalid JSON body")
    );
}
