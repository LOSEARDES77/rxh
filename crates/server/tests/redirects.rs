//! `rewrite`, `request.method`, `redirect` and the HTTP to HTTPS redirect.

mod util;

use std::net::SocketAddr;
use std::path::Path;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::header::LOCATION;
use hyper::{HeaderMap, Method, Request, StatusCode};
use rxh_test_util::free_address;
use tokio::net::TcpStream;

async fn get_host(address: SocketAddr, host: &str, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    let request = Request::get(uri)
        .header("host", host)
        .body(Empty::new())
        .expect("valid request");
    util::send(address, request).await
}

fn location(headers: &HeaderMap) -> &str {
    headers
        .get(LOCATION)
        .expect("Location header")
        .to_str()
        .expect("text")
}

fn tls_files(dir: &Path, host: &str) -> String {
    let certificate =
        rcgen::generate_simple_self_signed(vec![host.to_owned()]).expect("certificate");
    let cert = dir.join(format!("{host}.pem"));
    let key = dir.join(format!("{host}.key"));
    std::fs::write(&cert, certificate.cert.pem()).expect("write cert");
    std::fs::write(&key, certificate.signing_key.serialize_pem()).expect("write key");
    format!(
        "tls = {{ cert = {:?}, key = {:?} }}",
        cert.to_str().expect("UTF-8 path"),
        key.to_str().expect("UTF-8 path")
    )
}

#[tokio::test]
async fn rewrite_and_method_change_the_upstream_request() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmatch = [\n\
         {{ uri = \"/api\", rewrite = \"/v2{{path}}\", request = {{ method = \"post\" }}, \
            forward = \"{backend}\" }},\n\
         {{ uri = \"/find\", rewrite = \"/search?term={{query}}&host={{host}}\", \
            forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]\n"
    ))[0];

    // The query is kept when the rewrite has none, and the method replaced.
    let (status, _, _) = get_host(proxy, "a.test:8080", "/api/users?q=1").await;
    assert_eq!(status, StatusCode::OK);
    let seen = requests.recv().await.unwrap();
    assert_eq!(seen.uri, "/v2/api/users?q=1");
    assert_eq!(seen.method, Method::POST);

    // A rewrite with its own query replaces the query.
    get_host(proxy, "a.test:8080", "/find?rust").await;
    let seen = requests.recv().await.unwrap();
    assert_eq!(seen.uri, "/search?term=rust&host=a.test");
    assert_eq!(seen.method, Method::GET);

    // Routes without the options forward the request untouched.
    get_host(proxy, "a.test", "/other?x=1").await;
    let seen = requests.recv().await.unwrap();
    assert_eq!(seen.uri, "/other?x=1");
    assert_eq!(seen.method, Method::GET);
}

#[tokio::test]
async fn rewrite_that_yields_an_invalid_uri_answers_400() {
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nrewrite = \"/{{host}}{{path}}\"\n\
         forward = \"{backend}\"\n"
    ))[0];

    let (status, _, _) = get_host(proxy, "a b", "/x").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(requests.try_recv().is_err(), "nothing reaches the backend");

    get_host(proxy, "a.test", "/x").await;
    assert_eq!(requests.recv().await.unwrap().uri, "/a.test/x");
}

#[tokio::test]
async fn rewrite_and_method_apply_to_served_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("app.html"), "single page app").unwrap();
    let root = dir.path().to_str().unwrap();
    let server = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmatch = [\n\
         {{ path = \"/app/*\", rewrite = \"/app.html\", request = {{ method = \"GET\" }}, \
            serve = {root:?} }},\n\
         {{ serve = {root:?} }},\n]\n"
    ))[0];

    let (status, _, body) = util::get(server, "/app/settings/profile").await;
    assert_eq!(
        (status, &body[..]),
        (StatusCode::OK, &b"single page app"[..])
    );

    // POST is 405 on a file route, unless the route turns it into a GET.
    let post = |uri: &str| Request::post(uri).body(Empty::new()).unwrap();
    assert_eq!(util::send(server, post("/app/x")).await.0, StatusCode::OK);
    assert_eq!(
        util::send(server, post("/app.html")).await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(util::get(server, "/missing").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn redirect_routes_answer_with_a_location() {
    let backend = util::backend("backend").await;
    let server = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmatch = [\n\
         {{ uri = \"/old\", redirect = \"/new{{path}}?{{query}}\" }},\n\
         {{ host = \"www.a.test\", redirect = {{ to = \"http://{{host}}.example{{uri}}\", \
            status = 301 }} }},\n\
         {{ forward = \"{backend}\" }},\n]\n"
    ))[0];

    let (status, headers, body) = get_host(server, "a.test", "/old/page?q=1").await;
    assert_eq!(status, StatusCode::FOUND, "302 unless `status` is set");
    assert_eq!(location(&headers), "/new/old/page?q=1");
    assert!(body.is_empty());

    // `{host}` drops the port, `{uri}` keeps the query.
    let (status, headers, _) = get_host(server, "www.a.test:8080", "/x?y=z").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location(&headers), "http://www.a.test.example/x?y=z");

    // Requests that match no redirect are proxied.
    let (status, headers, body) = get_host(server, "a.test", "/page").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"backend"[..]));
    assert!(headers.get(LOCATION).is_none());
}

#[tokio::test]
async fn https_redirect_sends_plain_requests_to_the_tls_server() {
    let backend = util::backend("secure").await;
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_files(dir.path(), "a.test");
    let https = free_address();
    let http = SocketAddr::new(https.ip(), free_address().port());
    let config = |options: &str| {
        format!(
            "[acme]\nhttp_port = {}\n\
             [[server]]\nlisten = \"{https}\"\nhost = \"a.test\"\n{tls}\n\
             forward = \"{backend}\"\n{options}\n",
            http.port()
        )
    };

    // Off by default with certificate files: nothing listens for HTTP.
    util::spawn(&config(""));
    assert!(TcpStream::connect(http).await.is_err());

    util::spawn(&config("https_redirect = true"));
    let (status, headers, body) = get_host(http, "a.test", "/x/y?q=1").await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&headers),
        format!("https://a.test:{}/x/y?q=1", https.port())
    );
    assert!(body.is_empty());

    // The method does not matter, and only the hosts of the server redirect.
    let post = Request::post("/form")
        .header("host", format!("a.test:{}", http.port()))
        .body(Empty::new())
        .unwrap();
    let (status, headers, _) = util::send(http, post).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&headers),
        format!("https://a.test:{}/form", https.port())
    );
    assert_eq!(
        get_host(http, "other.test", "/").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn tls_auto_redirects_by_default_after_the_routes_of_plain_servers() {
    let backend = util::backend("plain").await;
    let https = free_address();
    let http = SocketAddr::new(https.ip(), free_address().port());
    let auto = |options: &str| {
        format!(
            "[acme]\nhttp_port = {}\n\
             [[server]]\nlisten = \"{https}\"\nhost = [\"a.test\", \"b.test\"]\n\
             tls = \"auto\"\nforward = \"{backend}\"\n{options}\n",
            http.port()
        )
    };

    // Opting out leaves the port alone.
    util::spawn(&auto("https_redirect = false"));
    assert!(TcpStream::connect(http).await.is_err());

    // A plain server on the HTTP port keeps its routes; the redirect takes
    // the requests for the TLS hosts that it does not match.
    util::spawn(&auto(&format!(
        "[[server]]\nlisten = \"{http}\"\npath = \"/health\"\nforward = \"{backend}\""
    )));
    for host in ["a.test", "b.test"] {
        let (status, headers, _) = get_host(http, host, "/login?next=/").await;
        assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            location(&headers),
            format!("https://{host}:{}/login?next=/", https.port())
        );
    }
    let (status, _, body) = get_host(http, "a.test", "/health").await;
    assert_eq!((status, &body[..]), (StatusCode::OK, &b"plain"[..]));
    assert_eq!(get_host(http, "c.test", "/").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn daemon_starts_the_https_redirect_of_its_configuration_file() {
    let backend = util::backend("secure").await;
    let dir = tempfile::tempdir().unwrap();
    let tls = tls_files(dir.path(), "a.test");
    let https = free_address();
    let http = SocketAddr::new(https.ip(), free_address().port());
    let config = dir.path().join("rxh.toml");
    std::fs::write(
        &config,
        format!(
            "[admin]\nenabled = false\n[acme]\nhttp_port = {}\n\
             [[server]]\nlisten = \"{https}\"\nhost = \"a.test\"\n{tls}\n\
             https_redirect = true\nforward = \"{backend}\"\n",
            http.port()
        ),
    )
    .unwrap();
    let daemon = tokio::task::spawn(rxh_server::Daemon::new(&config).run());

    util::eventually(async || TcpStream::connect(http).await.is_ok()).await;
    let (status, headers, _) = get_host(http, "a.test", "/x").await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(
        location(&headers),
        format!("https://a.test:{}/x", https.port())
    );
    daemon.abort();
}
