//! `handle_response` on proxy routes: `replace_status`, `copy_response`,
//! `serve` and `redirect` for matching upstream responses.

mod util;

use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};

/// Backend answering `/status/CODE` with that status and the body
/// `upstream CODE`. A `/file/NAME` path answers 200 with
/// `X-Accel-Redirect: /NAME`. Every response carries `x-upstream: yes`.
async fn backend() -> SocketAddr {
    util::backend_with(|request: Request<Incoming>| async move {
        let path = request.uri().path().to_owned();
        let mut response = Response::new(Full::<Bytes>::default());
        if let Some(code) = path.strip_prefix("/status/") {
            *response.status_mut() = code.parse().expect("status in the path");
            *response.body_mut() = Full::from(format!("upstream {code}"));
        } else if let Some(name) = path.strip_prefix("/file/") {
            let target = format!("/{name}").parse().expect("valid header");
            response.headers_mut().insert("x-accel-redirect", target);
            *response.body_mut() = Full::from("not for the client");
        }
        let headers = response.headers_mut();
        headers.insert("x-upstream", "yes".parse().expect("valid header"));
        response
    })
    .await
}

fn proxy(backend: SocketAddr, handlers: &str) -> SocketAddr {
    util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         handle_response = [\n{handlers}\n]\n"
    ))[0]
}

#[tokio::test]
async fn replace_status_and_copy_response_keep_the_upstream_response() {
    let proxy = proxy(
        backend().await,
        "{ status = 404, replace_status = 200 },\n\
         { status = 503, copy_response = true },\n\
         { status = 502, copy_response = 500 },\n\
         { status = \"5xx\", header = { x-upstream = \"yes\" }, replace_status = 299 },\n\
         { status = 403, header = { x-missing = \"*\" }, replace_status = 200 },",
    );

    // Only the status changes: body and headers are those of the backend.
    let (status, headers, body) = util::get(proxy, "/status/404").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "upstream 404");
    assert_eq!(headers["x-upstream"], "yes");

    // The first matching entry wins, so 503 is kept from the `5xx` entry.
    let (status, _, body) = util::get(proxy, "/status/503").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "upstream 503");

    let (status, _, body) = util::get(proxy, "/status/502").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, "upstream 502");

    let (status, _, body) = util::get(proxy, "/status/500").await;
    assert_eq!(status.as_u16(), 299);
    assert_eq!(body, "upstream 500");

    // Responses no entry matches pass untouched: other status, missing header.
    for code in [200, 403] {
        let (status, _, body) = util::get(proxy, &format!("/status/{code}")).await;
        assert_eq!(status.as_u16(), code);
        assert_eq!(body, format!("upstream {code}"));
    }
}

#[tokio::test]
async fn serve_replaces_the_upstream_response_with_a_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("503.html"), "maintenance page").expect("write");
    std::fs::write(dir.path().join("report.txt"), "private report").expect("write");
    let root = dir.path().to_str().expect("UTF-8 path");
    let proxy = proxy(
        backend().await,
        &format!(
            "{{ header = {{ x-accel-redirect = \"*\" }}, serve = {root:?}, \
                rewrite = \"{{header.x-accel-redirect}}\" }},\n\
             {{ status = \"5xx\", serve = {root:?}, rewrite = \"/{{status}}.html\" }},"
        ),
    );

    // An error page keeps the status of the error; nothing of the upstream
    // response reaches the client.
    let (status, headers, body) = util::get(proxy, "/status/503").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "maintenance page");
    assert!(!headers.contains_key("x-upstream"));

    // No page for this status: the file server answers 404.
    let (status, _, body) = util::get(proxy, "/status/500").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body, "upstream 500");

    // X-Accel-Redirect: the backend names the file, whatever the method.
    let request = Request::post("/file/report.txt")
        .body(Empty::new())
        .expect("valid request");
    let (status, headers, body) = util::send(proxy, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "private report");
    assert!(!headers.contains_key("x-accel-redirect"));

    // The backend cannot name a file outside the root.
    let (status, _, _) = util::get(proxy, "/file/..%2F..%2Fetc%2Fpasswd").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unmatched responses are proxied as usual.
    let (status, headers, body) = util::get(proxy, "/status/404").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "upstream 404");
    assert_eq!(headers["x-upstream"], "yes");
}

#[tokio::test]
async fn redirect_replaces_the_upstream_response() {
    let proxy = proxy(
        backend().await,
        "{ status = 401, redirect = { to = \"/login?from={path}&why={status}\", status = 303 } },",
    );

    let (status, headers, body) = util::get(proxy, "/status/401").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers["location"], "/login?from=/status/401&why=401");
    assert!(body.is_empty());

    let (status, _, body) = util::get(proxy, "/status/403").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "upstream 403");
}

#[tokio::test]
async fn errors_generated_by_the_proxy_are_not_handled() {
    // No backend answers, so there is no upstream response to handle.
    let proxy = proxy(
        util::dead_address(),
        "{ status = 502, replace_status = 200 },",
    );
    let (status, _, _) = util::get(proxy, "/").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
