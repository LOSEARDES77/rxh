//! `error_pages` on servers: files replacing the body of the error responses
//! RXH generates itself, never of upstream responses.

mod util;

use hyper::StatusCode;
use rxh_server::Master;

#[tokio::test]
async fn generated_errors_get_the_page_of_their_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let page = |name: &str, contents: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write");
        path.to_str().expect("UTF-8 path").to_owned()
    };
    let not_found = page("404.html", "<h1>missing</h1>");
    let client_error = page("4xx.txt", "client error");
    let server_error = page("50x.html", "<h1>down</h1>");
    let dead = util::dead_address();
    let upstream = util::status_backend(503).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\n\
         error_pages = {{ \"404\" = {not_found:?}, \"4xx\" = {client_error:?}, \
                          \"5xx\" = {server_error:?} }}\n\
         match = [\n\
           {{ uri = \"/dead\", forward = \"{dead}\" }},\n\
           {{ uri = \"/upstream\", forward = \"{upstream}\" }},\n\
           {{ uri = \"/denied\", ip_deny = [\"127.0.0.0/8\"], forward = \"{upstream}\" }},\n\
         ]\n"
    ))[0];

    // RXH could not reach the backend: the 5xx page, with the status kept.
    let (status, headers, body) = util::get(proxy, "/dead").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, "<h1>down</h1>");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    assert_eq!(headers["content-length"], "13");

    // No route matches: an exact code wins over its class.
    let (status, _, body) = util::get(proxy, "/nowhere").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "<h1>missing</h1>");

    let (status, headers, body) = util::get(proxy, "/denied").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "client error");
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");

    // The backend's own error passes untouched; that is `handle_response`.
    let (status, _, body) = util::get(proxy, "/upstream").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body, "status");
}

#[tokio::test]
async fn statuses_without_a_page_keep_the_default_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("502.html");
    std::fs::write(&path, "bad gateway page").expect("write");
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nuri = \"/app\"\nforward = \"{}\"\n\
         error_pages = {{ \"502\" = {:?} }}\n",
        util::dead_address(),
        path.to_str().expect("UTF-8 path"),
    ))[0];

    let (status, _, body) = util::get(proxy, "/nowhere").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, "HTTP 404 NOT FOUND");

    let (status, _, body) = util::get(proxy, "/app").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body, "bad gateway page");
}

#[tokio::test]
async fn a_missing_page_fails_the_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("missing.html");
    let missing = missing.to_str().expect("UTF-8 path");
    let config = util::config(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"127.0.0.1:1\"\n\
         error_pages = {{ \"5xx\" = {missing:?} }}\n"
    ));
    let Err(err) = Master::from_config(&config) else {
        panic!("a server with an unreadable error page must not start");
    };
    let err = err.to_string();
    assert!(
        err.contains("error_pages") && err.contains(missing),
        "{err}"
    );
}
