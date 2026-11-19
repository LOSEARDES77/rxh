//! `basic_auth` and `forward_auth` on servers and match entries.

#![allow(clippy::unwrap_used)]

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU16;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::body::Incoming;
use hyper::{HeaderMap, Request, Response, StatusCode};
use tokio::sync::mpsc;

/// `GET uri` with the given headers.
async fn get(
    proxy: SocketAddr,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = Request::get(uri);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    util::send(proxy, request.body(Empty::<Bytes>::new()).unwrap()).await
}

async fn status(proxy: SocketAddr, uri: &str, headers: &[(&str, &str)]) -> StatusCode {
    get(proxy, uri, headers).await.0
}

/// `users` table with `name` and the hash of `password`.
fn users(name: &str, password: &str) -> String {
    // The lowest cost bcrypt has: the tests check the logic, not the work.
    let hash = bcrypt::hash(password, 4).unwrap();
    format!("users = {{ {name} = \"{hash}\" }}")
}

// base64 of "alice:secret", "alice:wrong", "bob:secret" and "root:toor".
const ALICE: &str = "Basic YWxpY2U6c2VjcmV0";
const ALICE_WRONG: &str = "Basic YWxpY2U6d3Jvbmc=";
const BOB: &str = "Basic Ym9iOnNlY3JldA==";
const ROOT: &str = "Basic cm9vdDp0b29y";

#[tokio::test]
async fn basic_auth_asks_for_credentials_and_keeps_others_from_the_backend() {
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         basic_auth = {{ realm = \"Staff\", {} }}",
        users("alice", "secret")
    ))[0];

    let (status, headers, _) = get(proxy, "/", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers["www-authenticate"],
        "Basic realm=\"Staff\", charset=\"UTF-8\""
    );

    for rejected in [
        ALICE_WRONG,
        BOB,
        "Bearer YWxpY2U6c2VjcmV0",
        "Basic",
        "Basic not base64!",
        // "alice", without a colon.
        "Basic YWxpY2U=",
    ] {
        assert_eq!(
            self::status(proxy, "/", &[("authorization", rejected)]).await,
            StatusCode::UNAUTHORIZED,
            "{rejected}"
        );
    }
    assert_eq!(counters.requests(), 0);

    // Twice: the second one is answered from the verified credentials.
    for _ in 0..2 {
        assert_eq!(
            self::status(proxy, "/", &[("authorization", ALICE)]).await,
            StatusCode::OK
        );
    }
    // The scheme is case insensitive, and a wrong password still fails
    // after a right one.
    let lower = ALICE.replace("Basic", "basic");
    assert_eq!(
        self::status(proxy, "/", &[("authorization", &lower)]).await,
        StatusCode::OK
    );
    assert_eq!(
        self::status(proxy, "/", &[("authorization", ALICE_WRONG)]).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(counters.requests(), 3);
}

#[tokio::test]
async fn basic_auth_of_a_route_replaces_the_one_of_the_server() {
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nbasic_auth = {{ {} }}\n\
         match = [\n\
         {{ uri = \"/admin\", basic_auth = {{ {} }}, forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]",
        users("alice", "secret"),
        users("root", "toor"),
    ))[0];
    let with = |uri: &'static str, credentials: &'static str| async move {
        status(proxy, uri, &[("authorization", credentials)]).await
    };

    assert_eq!(with("/", ALICE).await, StatusCode::OK);
    assert_eq!(with("/", ROOT).await, StatusCode::UNAUTHORIZED);
    assert_eq!(with("/admin", ROOT).await, StatusCode::OK);
    assert_eq!(with("/admin", ALICE).await, StatusCode::UNAUTHORIZED);
}

/// Auth service answering `status` with `headers` and the body `auth`, and
/// the heads of the requests it gets.
async fn auth_service(
    status: u16,
    headers: &'static [(&'static str, &'static str)],
) -> (SocketAddr, mpsc::Receiver<http::request::Parts>) {
    let (sender, receiver) = mpsc::channel(16);
    let address = util::backend_with(move |request: Request<Incoming>| {
        let sender = sender.clone();
        async move {
            let _ = sender.send(request.into_parts().0).await;
            let mut response = Response::new(Full::from("auth"));
            *response.status_mut() = StatusCode::from_u16(status).unwrap();
            for (name, value) in headers {
                response.headers_mut().append(*name, value.parse().unwrap());
            }
            response
        }
    })
    .await;
    (address, receiver)
}

#[tokio::test]
async fn forward_auth_describes_the_request_and_copies_headers_to_the_backend() {
    let (auth, mut checks) =
        auth_service(200, &[("remote-user", "alice"), ("x-internal", "secret")]).await;
    let (backend, mut requests) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         rewrite = \"/internal\"\n\
         forward_auth = {{ url = \"http://{auth}/verify?kind=proxy\", \
         copy_headers = [\"Remote-User\", \"Remote-Groups\"] }}"
    ))[0];

    let request = Request::post("/app/page?x=1")
        .header("host", "app.example.com")
        .header("cookie", "session=abc")
        .header("content-length", "0")
        // None of these may be believed.
        .header("remote-user", "admin")
        .header("remote-groups", "wheel")
        .header("x-forwarded-uri", "/public")
        .header("x-forwarded-method", "GET")
        .header("x-forwarded-for", "10.9.9.9")
        .header("x-forwarded-host", "other.example.com")
        .body(Empty::<Bytes>::new())
        .unwrap();
    let (status, _, body) = util::send(proxy, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "intercepted");

    let check = checks.recv().await.unwrap();
    assert_eq!(check.method, "GET");
    assert_eq!(check.uri, "/verify?kind=proxy");
    assert_eq!(check.headers["host"], auth.to_string().as_str());
    assert_eq!(check.headers["x-forwarded-method"], "POST");
    assert_eq!(check.headers["x-forwarded-uri"], "/app/page?x=1");
    assert_eq!(check.headers["x-forwarded-host"], "app.example.com");
    assert_eq!(check.headers["x-forwarded-proto"], "http");
    assert_eq!(check.headers["x-forwarded-for"], "127.0.0.1");
    assert_eq!(check.headers["cookie"], "session=abc");

    let forwarded = requests.recv().await.unwrap();
    assert_eq!(forwarded.method, "POST");
    assert_eq!(forwarded.uri, "/internal?x=1");
    assert_eq!(forwarded.headers["remote-user"], "alice");
    assert!(
        !forwarded.headers.contains_key("remote-groups"),
        "a copied header the service did not send is removed"
    );
    assert!(
        !forwarded.headers.contains_key("x-internal"),
        "only the listed headers are copied"
    );
    assert_eq!(forwarded.headers["cookie"], "session=abc");
}

#[tokio::test]
async fn forward_auth_answers_of_the_service_go_to_the_client() {
    let (login, _login_checks) = auth_service(
        302,
        &[
            ("location", "https://auth.example.com/?rd=https://app/"),
            ("set-cookie", "state=1"),
        ],
    )
    .await;
    let (denied, _denied_checks) = auth_service(401, &[("www-authenticate", "Bearer")]).await;
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\n\
         forward_auth = {{ url = \"http://{login}\" }}\n\
         match = [\n\
         {{ uri = \"/api\", forward_auth = {{ url = \"http://{denied}/\" }}, forward = \"{backend}\" }},\n\
         {{ forward = \"{backend}\" }},\n]"
    ))[0];

    let (status, headers, body) = get(proxy, "/", &[]).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        headers["location"],
        "https://auth.example.com/?rd=https://app/"
    );
    assert_eq!(headers["set-cookie"], "state=1");
    assert_eq!(body, "auth");

    // The route has its own service instead of the server's.
    let (status, headers, _) = get(proxy, "/api", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers["www-authenticate"], "Bearer");
    assert_eq!(counters.requests(), 0);
}

#[tokio::test]
async fn forward_auth_fails_closed_when_the_service_does() {
    let slow = util::backend_with(|_| async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        Response::new(Full::from("late"))
    })
    .await;
    let (backend, counters) = util::counting_backend(Arc::new(AtomicU16::new(200))).await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmatch = [\n\
         {{ uri = \"/down\", forward_auth = {{ url = \"http://{}\" }}, forward = \"{backend}\" }},\n\
         {{ uri = \"/slow\", forward_auth = {{ url = \"http://{slow}\", timeout = \"100ms\" }}, forward = \"{backend}\" }},\n]",
        util::dead_address()
    ))[0];

    assert_eq!(status(proxy, "/down", &[]).await, StatusCode::BAD_GATEWAY);
    assert_eq!(
        status(proxy, "/slow", &[]).await,
        StatusCode::GATEWAY_TIMEOUT
    );
    assert_eq!(counters.requests(), 0);
}

#[tokio::test]
async fn basic_auth_is_checked_before_forward_auth() {
    let (auth, mut checks) = auth_service(200, &[]).await;
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         basic_auth = {{ {} }}\nforward_auth = {{ url = \"http://{auth}\" }}",
        users("alice", "secret")
    ))[0];

    assert_eq!(status(proxy, "/", &[]).await, StatusCode::UNAUTHORIZED);
    assert!(checks.try_recv().is_err(), "the service was not asked");

    assert_eq!(
        status(proxy, "/", &[("authorization", ALICE)]).await,
        StatusCode::OK
    );
    assert_eq!(checks.recv().await.unwrap().headers["authorization"], ALICE);
}

#[tokio::test]
async fn forward_auth_keeps_forwarded_headers_of_trusted_proxies() {
    let (auth, mut checks) = auth_service(200, &[]).await;
    let backend = util::backend("ok").await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         trusted_proxies = [\"127.0.0.1\"]\nforward_auth = {{ url = \"http://{auth}\" }}"
    ))[0];

    let headers = [
        ("x-forwarded-for", "10.0.0.7"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-uri", "/forged"),
    ];
    assert_eq!(status(proxy, "/real", &headers).await, StatusCode::OK);
    let check = checks.recv().await.unwrap();
    assert_eq!(check.headers["x-forwarded-for"], "10.0.0.7, 127.0.0.1");
    assert_eq!(check.headers["x-forwarded-proto"], "https");
    assert_eq!(check.headers["x-forwarded-uri"], "/real");
}
