//! Accounts of the admin API: first-run setup, sign-in, sessions, and the
//! gate that keeps a user without the `admin` role out of the admin routes.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use hyper::{HeaderMap, Method, StatusCode, header};
use rxh_server::access::{Access, Grant, Permission, Role, Target, User};
use rxh_server::{ADMIN_ROUTES, Audience};
use rxh_test_util::free_address;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use util::admin::{call, call_with_cookie, get, send, session_cookie, sign_in, start};

const TOKEN: &str = "s3cret-admin-token-0123";
const NOT_LOCAL: &str = "admin API only accepts a loopback Host and its own Origin";
const NEEDS_ADMIN: &str = "this needs the admin role";

/// A running daemon and its users file.
struct Fixture {
    admin: SocketAddr,
    users: PathBuf,
    _directory: TempDir,
    _daemon: JoinHandle<Result<(), rxh_server::Error>>,
}

/// Starts a daemon whose users file holds `access` (no file for `None`),
/// with `[admin] token` when `token` is set.
async fn daemon(access: Option<&Access>, token: bool) -> Fixture {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temporary directory");
    let users = directory.path().join("data").join("users.toml");
    if let Some(access) = access {
        access.save(&users).expect("write the users file");
    }
    let token = if token {
        format!("token = \"{TOKEN}\"\n")
    } else {
        String::new()
    };
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\nusers = {users:?}\n{token}\n\
             [upstream.api]\nbackends = [\"127.0.0.1:9\"]\n"
        ),
    )
    .expect("write the configuration");
    let daemon = start(&config_path, admin).await;
    Fixture {
        admin,
        users,
        _directory: directory,
        _daemon: daemon,
    }
}

/// A user whose hash has the lowest cost, so that signing in is fast.
fn user(name: &str, password: &str, roles: &[&str]) -> User {
    User {
        name: name.to_owned(),
        password: bcrypt::hash(password, 4).expect("hash"),
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

/// `ana`, an administrator, and `luis`, who may view and create hosts of
/// the group `web`.
fn accounts() -> Access {
    Access {
        user: vec![
            user("ana", "password-ana", &["admin"]),
            user("luis", "password-luis", &["web-editors"]),
        ],
        role: vec![Role {
            name: "web-editors".to_owned(),
            grant: vec![Grant {
                target: Target::Group("web".to_owned()),
                allow: vec![Permission::View, Permission::Create],
            }],
        }],
    }
}

/// Signs in and returns the session cookie as `name=value`.
async fn cookie_of(admin: SocketAddr, name: &str, password: &str) -> String {
    let (status, headers, json) = sign_in(admin, name, password).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    session_cookie(&headers)
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

#[tokio::test(flavor = "multi_thread")]
async fn before_any_user_the_api_behaves_as_before() {
    // Without a token: a local request is the administrator, and what a page
    // of another site sends is refused, the dashboard and `/auth` included.
    let open = daemon(None, /* token= */ false).await;
    let (status, json) = get(open.admin, "/status").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let (status, json) = send(open.admin, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let stale = [(header::COOKIE, "rxh_session=not-a-session")];
    let (status, _, json) = call(open.admin, Method::GET, "/pools", "", &stale).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let foreign = [(header::HOST, "evil.example")];
    for (method, uri) in [
        (Method::GET, "/status"),
        (Method::GET, "/dashboard"),
        (Method::GET, "/auth/session"),
        (Method::POST, "/auth/login"),
        (Method::POST, "/auth/setup"),
    ] {
        let (status, _, json) = call(open.admin, method.clone(), uri, "{}", &foreign).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(json["error"], NOT_LOCAL, "{method} {uri}");
    }
    assert!(!open.users.exists(), "nothing wrote the users file");

    // With a token: nothing without it but the page, and everything with it.
    let closed = daemon(None, /* token= */ true).await;
    let (status, headers, json) = call(closed.admin, Method::GET, "/status", "", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
    assert_eq!(
        json["error"],
        "admin API requires Authorization: Bearer <token>"
    );
    let (status, _, _) = util::get(closed.admin, "/dashboard").await;
    assert_eq!(status, StatusCode::OK);
    let bearer = bearer();
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let (status, _, json) = call(closed.admin, Method::GET, "/status", "", &authorized).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn session_reports_setup_while_no_user_exists() {
    let open = daemon(None, /* token= */ false).await;
    let (status, json) = get(open.admin, "/auth/session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        json!({
            "user": null,
            "admin": true,
            "roles": [],
            "grants": [],
            "creatable_groups": [],
            "setup": true,
            "setup_needs_code": false,
            "password_login": true,
            "oidc": false,
        })
    );

    // Signed out: the route still answers, and says who may not set up. The
    // code is for TLS connections, which this one is not.
    let closed = daemon(None, /* token= */ true).await;
    let (status, json) = get(closed.admin, "/auth/session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"], Value::Null);
    assert_eq!(json["admin"], false);
    assert_eq!(json["setup"], true);
    assert_eq!(json["setup_needs_code"], false);

    let with_users = daemon(Some(&accounts()), /* token= */ true).await;
    let (status, json) = get(with_users.admin, "/auth/session").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["setup"], false);
    assert_eq!(json["admin"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_creates_the_first_administrator_and_signs_in() {
    let open = daemon(None, /* token= */ false).await;
    let admin = open.admin;

    // What the users file refuses is refused here, and creates nothing.
    for (body, reason) in [
        (
            json!({ "name": "ana", "password": "short" }),
            "8 characters",
        ),
        (
            json!({ "name": "Ana B", "password": "password-ana" }),
            "name",
        ),
    ] {
        let (status, json) = send(admin, Method::POST, "/auth/setup", &body.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
        let error = json["error"].as_str().expect("error");
        assert!(error.contains(reason), "{error}");
        assert!(!open.users.exists());
    }
    let (status, json) = send(admin, Method::POST, "/auth/setup", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");

    let body = json!({ "name": "ana", "password": "password-ana" }).to_string();
    let (status, headers, json) = call(admin, Method::POST, "/auth/setup", &body, &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    let saved = Access::load(&open.users).expect("users file");
    assert_eq!(saved.user.len(), 1);
    assert_eq!(saved.user[0].name, "ana");
    assert_eq!(saved.user[0].roles, ["admin"]);
    assert!(saved.verify("ana", "password-ana"));

    // The answer signed the new user in.
    let cookie = session_cookie(&headers);
    let (status, _, json) =
        call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"], "ana");
    assert_eq!(json["admin"], true);
    assert_eq!(json["roles"], json!(["admin"]));
    assert_eq!(json["setup"], false);
    let (status, _, json) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_needs_an_administrator() {
    // With a token, a caller without it is nobody, and the setup code only
    // counts on a TLS connection.
    let closed = daemon(None, /* token= */ true).await;
    for body in [
        json!({ "name": "eva", "password": "password-eva" }),
        json!({ "name": "eva", "password": "password-eva", "code": "abcdefghjk" }),
        json!({ "name": "eva", "password": "password-eva", "code": "" }),
    ] {
        let body = body.to_string();
        let (status, json) = send(closed.admin, Method::POST, "/auth/setup", &body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
        assert_eq!(
            json["error"],
            "creating the first user needs an administrator or the setup code"
        );
        assert!(!closed.users.exists());
    }

    let bearer = bearer();
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let body = json!({ "name": "eva", "password": "password-eva" }).to_string();
    let (status, _, json) = call(
        closed.admin,
        Method::POST,
        "/auth/setup",
        &body,
        &authorized,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(
        Access::load(&closed.users).expect("file").user[0].name,
        "eva"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_is_refused_once_a_user_exists() {
    let open = daemon(Some(&accounts()), /* token= */ false).await;
    let before = fs::read_to_string(&open.users).expect("users file");
    let body = json!({ "name": "eva", "password": "password-eva" }).to_string();
    // A local request without a token is an administrator, and still refused.
    let (status, json) = send(open.admin, Method::POST, "/auth/setup", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json}");
    assert_eq!(json["error"], "a user exists already; sign in");
    assert_eq!(fs::read_to_string(&open.users).expect("users file"), before);
    let (status, _, _) = sign_in(open.admin, "eva", "password-eva").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_in_sets_a_private_cookie() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let (status, headers, json) = sign_in(closed.admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(json, Value::Null, "no body");
    let cookies: Vec<_> = headers.get_all(header::SET_COOKIE).iter().collect();
    assert_eq!(cookies.len(), 1);
    let cookie = cookies[0].to_str().expect("text");
    let (pair, attributes) = cookie.split_once(';').expect("attributes");
    let id = pair.strip_prefix("rxh_session=").expect("cookie name");
    assert_eq!(id.len(), 43, "{id}");
    // A plain connection from this machine: no `Secure`.
    assert_eq!(attributes, " HttpOnly; SameSite=Strict; Path=/");

    let bad = "{\"name\": \"ana\"}";
    let (status, json) = send(closed.admin, Method::POST, "/auth/login", bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_or_a_setup_from_the_page_of_another_site_is_refused() {
    let refused = json!({ "error": "the Origin of this request is not its Host" });
    let own = |admin: SocketAddr| format!("http://{admin}");

    // What a form of another site sends: its `Origin`, any content type.
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let login = json!({ "name": "ana", "password": "password-ana" }).to_string();
    for origin in ["https://evil.example", "null"] {
        let headers = [
            (header::ORIGIN, origin),
            (header::CONTENT_TYPE, "text/plain"),
        ];
        let (status, headers, json) =
            call(closed.admin, Method::POST, "/auth/login", &login, &headers).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{origin}");
        assert_eq!(json, refused, "{origin}");
        assert!(!headers.contains_key(header::SET_COOKIE), "{origin}");
    }
    // The page sends its own, and `curl` none.
    let origin = own(closed.admin);
    let headers = [(header::ORIGIN, origin.as_str())];
    let (status, _, json) = call(closed.admin, Method::POST, "/auth/login", &login, &headers).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    cookie_of(closed.admin, "ana", "password-ana").await;

    // Setup, with the token too: a browser that holds it is still a browser.
    let empty = daemon(None, /* token= */ true).await;
    let setup = json!({ "name": "eva", "password": "password-eva" }).to_string();
    let bearer = bearer();
    let headers = [
        (header::AUTHORIZATION, bearer.as_str()),
        (header::ORIGIN, "https://evil.example"),
    ];
    let (status, _, json) = call(empty.admin, Method::POST, "/auth/setup", &setup, &headers).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json, refused);
    assert!(!empty.users.exists());
    let origin = own(empty.admin);
    let headers = [
        (header::AUTHORIZATION, bearer.as_str()),
        (header::ORIGIN, origin.as_str()),
    ];
    let (status, _, json) = call(empty.admin, Method::POST, "/auth/setup", &setup, &headers).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert!(empty.users.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_password_and_an_unknown_name_get_the_same_answer() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let long = "x".repeat(65);
    let mut answers = Vec::new();
    for (name, password) in [
        ("ana", "password-luis"),
        ("nobody", "password-ana"),
        ("", ""),
        (long.as_str(), "password-ana"),
    ] {
        let (status, headers, json) = sign_in(closed.admin, name, password).await;
        assert!(!headers.contains_key(header::SET_COOKIE), "{name}");
        assert!(!headers.contains_key(header::WWW_AUTHENTICATE), "{name}");
        answers.push((status, json));
    }
    let wrong = json!({ "error": "wrong name or password" });
    for answer in &answers {
        assert_eq!(*answer, (StatusCode::UNAUTHORIZED, wrong.clone()));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn five_failures_lock_the_name_for_that_client() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    for attempt in 1..=5 {
        let (status, _, _) = sign_in(closed.admin, "luis", "not-the-password").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    // Not even the right password now.
    let (status, headers, json) = sign_in(closed.admin, "luis", "password-luis").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!headers.contains_key(header::SET_COOKIE));
    assert_eq!(
        json["error"],
        "too many sign-in attempts; try again in a minute"
    );
    // Another name from the same client is not locked.
    cookie_of(closed.admin, "ana", "password-ana").await;

    // Ten attempts a minute for one client, whatever the names: three are
    // left, and the eleventh is refused with the right password too.
    for name in ["eva", "noa", "teo"] {
        let (status, _, _) = sign_in(closed.admin, name, "not-the-password").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{name}");
    }
    let (status, headers, json) = sign_in(closed.admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!headers.contains_key(header::SET_COOKIE));
    assert_eq!(
        json["error"],
        "too many sign-in attempts; try again in a minute"
    );
    // The token is not a sign-in.
    let bearer = bearer();
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let (status, _, json) = call(closed.admin, Method::GET, "/status", "", &authorized).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_cookie_identifies_the_user() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let session = async || {
        let (status, _, json) =
            call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
        assert_eq!(status, StatusCode::OK);
        json
    };
    assert_eq!(
        session().await,
        json!({
            "user": "luis",
            "admin": false,
            "roles": ["web-editors"],
            "grants": [{ "group": "web", "allow": ["view", "create"] }],
            "creatable_groups": ["web"],
            "setup": false,
            "setup_needs_code": false,
            "password_login": true,
            "oidc": false,
        })
    );
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"], NEEDS_ADMIN);

    // The roles are read on every request: a change applies to the session.
    let mut promoted = accounts();
    promoted.user[1].roles = vec!["admin".to_owned()];
    promoted.save(&closed.users).expect("save");
    util::eventually(async || session().await["admin"] == true).await;
    let promoted = session().await;
    assert_eq!(promoted["roles"], json!(["admin"]));
    assert_eq!(promoted["grants"], json!([]));
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // A session of a user that was removed is nobody's.
    let mut removed = accounts();
    removed.user.truncate(1);
    removed.save(&closed.users).expect("save");
    util::eventually(async || session().await["user"] == Value::Null).await;
    assert_eq!(session().await["admin"], false);
    let (status, _, _) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A cookie that names no session, with and without the token.
    let made_up = format!("rxh_session={}", "A".repeat(43));
    let (status, headers, _) = call_with_cookie(admin, &made_up, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_ends_the_session() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let cookie = cookie_of(admin, "ana", "password-ana").await;
    let other = cookie_of(admin, "ana", "password-ana").await;
    assert_ne!(cookie, other);
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    let (status, headers, _) =
        call_with_cookie(admin, &cookie, Method::POST, "/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers[header::SET_COOKIE],
        "rxh_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    );
    let (status, _, _) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, json) =
        call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"], Value::Null);
    // Only that session ended.
    let (status, _, json) = call_with_cookie(admin, &other, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // Signed out, there is nothing to end: the cookie is cleared all the same.
    let (status, headers, _) = call(admin, Method::POST, "/auth/logout", "", &[]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers.contains_key(header::SET_COOKIE));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cookie_write_without_matching_origin_is_forbidden() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let cookie = cookie_of(admin, "ana", "password-ana").await;
    let other_port = format!("http://127.0.0.1:{}", admin.port().wrapping_add(1));
    let origins: [&[(header::HeaderName, &str)]; 5] = [
        &[],
        &[(header::ORIGIN, "http://evil.example")],
        &[(header::ORIGIN, "null")],
        &[(header::ORIGIN, other_port.as_str())],
        // Equal to each other, but not the `Origin` of the `Host`.
        &[
            (header::HOST, "evil.example"),
            (header::ORIGIN, "http://rxh.example"),
        ],
    ];
    for origin in origins {
        for (method, uri) in [
            (Method::POST, "/reload"),
            (Method::POST, "/stop"),
            (Method::POST, "/auth/logout"),
            (Method::PUT, "/auth/password"),
            (Method::DELETE, "/pools/api/upstreams?address=127.0.0.1:9"),
        ] {
            let mut headers = vec![(header::COOKIE, cookie.as_str())];
            headers.extend_from_slice(origin);
            let (status, _, json) = call(admin, method.clone(), uri, "", &headers).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri} {origin:?}");
            assert_eq!(
                json["error"],
                "a request with a session cookie needs an Origin equal to its Host"
            );
        }
    }

    // Reading needs no `Origin`: nothing changed, the session is still there.
    let with_cookie = [(header::COOKIE, cookie.as_str())];
    let (status, _, pool) = call(admin, Method::GET, "/pools/api", "", &with_cookie).await;
    assert_eq!(status, StatusCode::OK, "{pool}");
    assert_eq!(pool["upstreams"][0]["address"], "127.0.0.1:9");
    // The page sends its own origin, under whatever name it was reached.
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let named = [
        (header::COOKIE, cookie.as_str()),
        (header::HOST, "rxh.example:2019"),
        (header::ORIGIN, "https://rxh.example:2019"),
    ];
    let (status, _, json) = call(admin, Method::POST, "/reload", "", &named).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn changing_the_password_needs_the_current_one_and_ends_other_sessions() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let other = cookie_of(admin, "luis", "password-luis").await;
    let ana = cookie_of(admin, "ana", "password-ana").await;
    let change = async |cookie: &str, current: &str, new: &str| {
        let body = json!({ "current": current, "new": new }).to_string();
        call_with_cookie(admin, cookie, Method::PUT, "/auth/password", &body).await
    };

    let (status, _, json) = change(&cookie, "password-ana", "new-password").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"], "the current password is wrong");
    let (status, _, json) = change(&cookie, "password-luis", "short").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    // The token is no user, and has no password to change.
    let bearer = bearer();
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let body = json!({ "current": "password-luis", "new": "new-password" }).to_string();
    let (status, _, _) = call(admin, Method::PUT, "/auth/password", &body, &authorized).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(admin, Method::PUT, "/auth/password", &body, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let session = async |cookie: &str| {
        let (_, _, json) = call_with_cookie(admin, cookie, Method::GET, "/auth/session", "").await;
        json["user"].clone()
    };
    assert_eq!(session(&other).await, "luis", "nothing changed so far");

    let (status, headers, json) = change(&cookie, "password-luis", "new-password").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    let renewed = session_cookie(&headers);
    assert_ne!(renewed, cookie);
    assert_eq!(session(&renewed).await, "luis");
    assert_eq!(session(&other).await, Value::Null);
    assert_eq!(session(&cookie).await, Value::Null);
    assert_eq!(session(&ana).await, "ana", "another user stays signed in");

    let (status, _, _) = sign_in(admin, "luis", "password-luis").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    cookie_of(admin, "luis", "new-password").await;
    let saved = Access::load(&closed.users).expect("users file");
    assert_eq!(saved.user[1].roles, ["web-editors"], "only the password");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_still_is_administrator_when_users_exist() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let bearer = bearer();
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    for (method, uri) in [
        (Method::GET, "/status"),
        (Method::GET, "/config"),
        (Method::POST, "/reload"),
    ] {
        let (status, _, json) = call(admin, method.clone(), uri, "", &authorized).await;
        assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
        let (status, headers, _) = call(admin, method.clone(), uri, "", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
    }
    let (_, _, json) = call(admin, Method::GET, "/auth/session", "", &authorized).await;
    assert_eq!(json["user"], Value::Null);
    assert_eq!(json["admin"], true);

    // The token wins over the cookie of a user without the role, and needs
    // no `Origin`.
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let both = [
        (header::AUTHORIZATION, bearer.as_str()),
        (header::COOKIE, cookie.as_str()),
    ];
    let (status, _, json) = call(admin, Method::POST, "/reload", "", &both).await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_request_still_is_administrator_when_users_exist() {
    let open = daemon(Some(&accounts()), /* token= */ false).await;
    let admin = open.admin;
    for (method, uri) in [
        (Method::GET, "/status"),
        (Method::GET, "/config"),
        (Method::POST, "/reload"),
    ] {
        let (status, json) = send(admin, method.clone(), uri, "").await;
        assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
    }
    let (_, json) = get(admin, "/auth/session").await;
    assert_eq!(json["user"], Value::Null);
    assert_eq!(json["admin"], true);
    assert_eq!(json["setup"], false);

    // A session comes before the local rule: the browser of a user without
    // the role, on the machine, gets what the user may do.
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"], NEEDS_ADMIN);
    // A session of a user that was removed is nobody's, also here. Signing
    // out ends it, and the browser is the machine again.
    let mut removed = accounts();
    removed.user.truncate(1);
    removed.save(&open.users).expect("save");
    let status_of = async || {
        let (status, _, _) = call_with_cookie(admin, &cookie, Method::GET, "/status", "").await;
        status
    };
    util::eventually(async || status_of().await == StatusCode::UNAUTHORIZED).await;
    let with_cookie = [(header::COOKIE, cookie.as_str())];
    let (status, _, _) = call(admin, Method::POST, "/auth/logout", "", &with_cookie).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a cookie write needs its Origin"
    );
    let (status, headers, _) =
        call_with_cookie(admin, &cookie, Method::POST, "/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        headers[header::SET_COOKIE],
        "rxh_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"
    );
    assert_eq!(status_of().await, StatusCode::OK);

    // And a foreign `Host` is still refused, as before the accounts.
    let foreign = [(header::HOST, "evil.example")];
    let (status, _, json) = call(admin, Method::GET, "/status", "", &foreign).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json["error"], NOT_LOCAL);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_without_admin_is_forbidden_on_every_admin_route() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let luis = cookie_of(admin, "luis", "password-luis").await;
    let ana = cookie_of(admin, "ana", "password-ana").await;

    let routes: Vec<(Method, String)> = ADMIN_ROUTES
        .iter()
        .filter(|(_, _, audience)| *audience == Audience::Admin)
        .map(|(method, path, _)| {
            let method: Method = method.parse().expect("method");
            (method, format!("/{}", path.replace("{}", "x")))
        })
        .collect();
    assert!(routes.len() > 20, "the table lists the routes");
    assert!(routes.contains(&(Method::POST, "/stop".to_owned())));
    // What the router does not know is for administrators too.
    let unknown = [
        (Method::GET, "/nope".to_owned()),
        (Method::GET, "/users/x/y".to_owned()),
        (Method::DELETE, "/auth/session".to_owned()),
        (Method::GET, "/dashboard/x".to_owned()),
    ];
    for (method, uri) in routes.iter().chain(&unknown) {
        let (status, _, json) = call_with_cookie(admin, &luis, method.clone(), uri, "{}").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert_eq!(json["error"], NEEDS_ADMIN, "{method} {uri}");
        // Signed out it is `401`.
        let (status, _, _) = call(admin, method.clone(), uri, "{}", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }

    // None of them ran (`/stop` is one), and the role opens every one.
    for (method, uri) in &routes {
        if *method != Method::GET {
            continue;
        }
        let (status, _, json) = call_with_cookie(admin, &ana, method.clone(), uri, "").await;
        assert!(
            matches!(status, StatusCode::OK | StatusCode::NOT_FOUND),
            "{method} {uri}: {status} {json}"
        );
    }
    let (status, _, json) = call_with_cookie(admin, &ana, Method::POST, "/reload", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // The routes written for every signed-in user answer luis.
    let (status, _, _) = call_with_cookie(admin, &luis, Method::GET, "/auth/session", "").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = call_with_cookie(admin, &luis, Method::POST, "/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn hashes_and_session_ids_never_appear_in_responses() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let hashes: Vec<String> = accounts_on_disk(&closed)
        .user
        .into_iter()
        .map(|user| user.password)
        .collect();
    assert!(hashes.iter().all(|hash| hash.starts_with("$2")));

    let mut answers: Vec<(String, HeaderMap, Value)> = Vec::new();
    let (_, headers, json) = sign_in(admin, "ana", "password-ana").await;
    let ana = session_cookie(&headers);
    answers.push(("sign-in".to_owned(), HeaderMap::new(), json));
    let luis = cookie_of(admin, "luis", "password-luis").await;
    let ids: Vec<&str> = [&ana, &luis]
        .iter()
        .map(|cookie| cookie.strip_prefix("rxh_session=").expect("cookie"))
        .collect();

    for cookie in [&ana, &luis] {
        for uri in [
            "/auth/session",
            "/status",
            "/config",
            "/config/files",
            "/pools",
            "/stats/history",
            "/nope",
        ] {
            let (_, headers, json) = call_with_cookie(admin, cookie, Method::GET, uri, "").await;
            answers.push((uri.to_owned(), headers, json));
        }
        let body = json!({ "current": "wrong", "new": "x" }).to_string();
        let (_, headers, json) =
            call_with_cookie(admin, cookie, Method::PUT, "/auth/password", &body).await;
        answers.push(("password".to_owned(), headers, json));
    }
    let (_, headers, json) = sign_in(admin, "ana", "wrong-password").await;
    answers.push(("failed sign-in".to_owned(), headers, json));
    let (_, headers, json) = send_setup(admin).await;
    answers.push(("setup".to_owned(), headers, json));

    assert!(answers.len() > 15);
    for (route, headers, json) in &answers {
        let text = format!("{headers:?} {json}");
        for secret in hashes.iter().map(String::as_str).chain(ids.iter().copied()) {
            assert!(!text.contains(secret), "{route}: {text}");
        }
        assert!(!text.contains("$2b$"), "{route}: {text}");
    }
}

fn accounts_on_disk(fixture: &Fixture) -> Access {
    Access::load(&fixture.users).expect("users file")
}

async fn send_setup(admin: SocketAddr) -> (StatusCode, HeaderMap, Value) {
    let body = json!({ "name": "eva", "password": "password-eva" }).to_string();
    call(admin, Method::POST, "/auth/setup", &body, &[]).await
}

/// A request to the admin API with the token: status and JSON body.
async fn with_token(
    admin: SocketAddr,
    method: Method,
    uri: &str,
    body: Value,
) -> (StatusCode, Value) {
    let bearer = bearer();
    let body = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let (status, _, json) = call(
        admin,
        method,
        uri,
        &body,
        &[(header::AUTHORIZATION, bearer.as_str())],
    )
    .await;
    (status, json)
}

/// The names and roles of the users on disk.
fn users_on_disk(fixture: &Fixture) -> Vec<(String, Vec<String>)> {
    accounts_on_disk(fixture)
        .user
        .into_iter()
        .map(|user| (user.name, user.roles))
        .collect()
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_administrator_creates_lists_changes_and_removes_users() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let ana = cookie_of(admin, "ana", "password-ana").await;
    let as_ana = async |method: Method, uri: &str, body: Value| {
        let (status, _, json) = call_with_cookie(admin, &ana, method, uri, &body.to_string()).await;
        (status, json)
    };

    let eva = json!({ "name": "eva", "password": "password-eva", "roles": ["web-editors"] });
    let (status, json) = as_ana(Method::POST, "/users", eva.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json, json!({ "name": "eva", "roles": ["web-editors"] }));
    assert!(accounts_on_disk(&closed).verify("eva", "password-eva"));
    let (status, json) = as_ana(Method::GET, "/users", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        json!([
            { "name": "ana", "roles": ["admin"] },
            { "name": "luis", "roles": ["web-editors"] },
            { "name": "eva", "roles": ["web-editors"] },
        ])
    );

    // Refused by the rules of the users file: `409` with their reason.
    let (status, json) = as_ana(Method::POST, "/users", eva).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "user \"eva\" exists");
    let short = json!({ "name": "rosa", "password": "short", "roles": [] });
    let (status, json) = as_ana(Method::POST, "/users", short).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        json["error"],
        "a password has at least 8 characters and at most 72 bytes"
    );
    let unknown_role = json!({ "roles": ["nope"] });
    let (status, json) = as_ana(Method::PUT, "/users/eva", unknown_role).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "user \"eva\": no role \"nope\"");
    // Invalid bodies.
    for body in [
        json!({ "name": "rosa", "roles": [] }),
        json!({ "name": "rosa", "password": "password-rosa", "role": ["admin"] }),
        json!("rosa"),
    ] {
        let (status, json) = as_ana(Method::POST, "/users", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json}");
    }
    let (status, _) = as_ana(Method::PUT, "/users/eva", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(users_on_disk(&closed).len(), 3, "nothing was added");

    let (status, json) = as_ana(Method::PUT, "/users/eva", json!({ "roles": ["admin"] })).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(
        users_on_disk(&closed)[2],
        ("eva".to_owned(), names(&["admin"]))
    );
    let both = json!({ "roles": [], "password": "another-password" });
    let (status, json) = as_ana(Method::PUT, "/users/eva", both).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    let saved = accounts_on_disk(&closed);
    assert!(saved.verify("eva", "another-password"));
    assert!(saved.user[2].roles.is_empty());

    // Unknown names.
    let (status, json) = as_ana(Method::PUT, "/users/rosa", json!({ "roles": [] })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "no user \"rosa\"");
    let (status, _) = as_ana(Method::DELETE, "/users/rosa", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, json) = as_ana(Method::DELETE, "/users/eva", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(
        users_on_disk(&closed),
        [
            ("ana".to_owned(), names(&["admin"])),
            ("luis".to_owned(), names(&["web-editors"]))
        ]
    );
    let (status, _) = as_ana(Method::DELETE, "/users/eva", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_user_can_sign_in_and_has_its_grants_in_the_session() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let eva = json!({ "name": "eva", "password": "password-eva", "roles": ["web-editors"] });
    let (status, json) = with_token(admin, Method::POST, "/users", eva).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");

    let cookie = cookie_of(admin, "eva", "password-eva").await;
    let (status, _, json) =
        call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["user"], "eva");
    assert_eq!(json["admin"], false);
    assert_eq!(json["roles"], json!(["web-editors"]));
    assert_eq!(
        json["grants"],
        json!([{ "group": "web", "allow": ["view", "create"] }])
    );
    let (status, _, _) = call_with_cookie(admin, &cookie, Method::GET, "/users", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_user_ends_its_sessions() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let session_user = async |cookie: &str| {
        let (_, _, json) = call_with_cookie(admin, cookie, Method::GET, "/auth/session", "").await;
        json["user"].clone()
    };

    // Removed and created again with the same name: the old session stays
    // ended.
    let old = cookie_of(admin, "luis", "password-luis").await;
    assert_eq!(session_user(&old).await, "luis");
    let (status, json) = with_token(admin, Method::DELETE, "/users/luis", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    let luis = json!({ "name": "luis", "password": "password-luis", "roles": ["web-editors"] });
    let (status, json) = with_token(admin, Method::POST, "/users", luis).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(session_user(&old).await, Value::Null);

    // A new password ends the sessions too; new roles do not.
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let (status, _) = with_token(admin, Method::PUT, "/users/luis", json!({ "roles": [] })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(session_user(&cookie).await, "luis");
    let password = json!({ "password": "another-password" });
    let (status, _) = with_token(admin, Method::PUT, "/users/luis", password).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(session_user(&cookie).await, Value::Null);
    let (status, _, _) = sign_in(admin, "luis", "another-password").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_administrator_cannot_be_removed_through_the_api() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let before = users_on_disk(&closed);
    let (status, json) = with_token(admin, Method::DELETE, "/users/ana", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "\"ana\" is the last administrator");
    let demoted = json!({ "roles": ["web-editors"] });
    let (status, json) = with_token(admin, Method::PUT, "/users/ana", demoted.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "\"ana\" is the last administrator");
    assert_eq!(users_on_disk(&closed), before);

    // With a second administrator the first one may go.
    let (status, _) = with_token(
        admin,
        Method::PUT,
        "/users/luis",
        json!({ "roles": ["admin"] }),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, json) = with_token(admin, Method::PUT, "/users/ana", demoted).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn roles_are_created_replaced_and_removed() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let roles_on_disk = || accounts_on_disk(&closed).role;

    let (status, json) = with_token(admin, Method::GET, "/roles", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        json!([
            { "name": "admin", "builtin": true },
            { "name": "web-editors", "grant": [{ "group": "web", "allow": ["view", "create"] }] },
        ])
    );

    let ops = json!({ "name": "ops", "grant": [{ "host": "shop", "allow": ["view", "modify"] }] });
    let (status, json) = with_token(admin, Method::POST, "/roles", ops.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json, ops);
    assert_eq!(roles_on_disk()[1].name, "ops");
    assert_eq!(
        roles_on_disk()[1].grant,
        [Grant {
            target: Target::Host("shop".to_owned()),
            allow: vec![Permission::View, Permission::Modify],
        }]
    );

    // Refusals of the users file, and invalid bodies.
    for (body, message) in [
        (ops, "role \"ops\" exists"),
        (
            json!({ "name": "admin" }),
            "role \"admin\" is built in and cannot be defined",
        ),
        (
            json!({ "name": "x", "grant": [{ "host": "shop", "allow": ["create"] }] }),
            "role \"x\": `create` is for groups, not for host \"shop\"",
        ),
    ] {
        let (status, json) = with_token(admin, Method::POST, "/roles", body.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(json["error"], message, "{body}");
    }
    for body in [
        json!({ "grant": [] }),
        json!({ "name": "x", "grant": [{ "group": "web", "host": "shop", "allow": [] }] }),
        json!({ "name": "x", "grant": [{ "group": "web", "allow": ["own"] }] }),
        json!({ "name": "x", "grants": [] }),
    ] {
        let (status, json) = with_token(admin, Method::POST, "/roles", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json}");
    }
    assert_eq!(roles_on_disk().len(), 2);

    // Replaced: the name in the body is optional and must match the path.
    let api = json!({ "grant": [{ "group": "api", "allow": ["view"] }] });
    let (status, json) = with_token(admin, Method::PUT, "/roles/ops", api).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(
        roles_on_disk()[1].grant,
        [Grant {
            target: Target::Group("api".to_owned()),
            allow: vec![Permission::View],
        }]
    );
    let renamed = json!({ "name": "other", "grant": [] });
    let (status, _) = with_token(admin, Method::PUT, "/roles/ops", renamed).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let empty = json!({ "name": "ops", "grant": [] });
    let (status, _) = with_token(admin, Method::PUT, "/roles/ops", empty.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(roles_on_disk()[1].grant.is_empty());
    let (status, json) = with_token(admin, Method::PUT, "/roles/nope", json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "no role \"nope\"");
    let (status, _) = with_token(admin, Method::PUT, "/roles/admin", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, json) = with_token(admin, Method::DELETE, "/roles/ops", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(roles_on_disk().len(), 1);
    let (status, _) = with_token(admin, Method::DELETE, "/roles/ops", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, json) = with_token(admin, Method::DELETE, "/roles/admin", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        json["error"],
        "role \"admin\" is built in and cannot be removed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_role_in_use_is_not_removed() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let (status, json) = with_token(admin, Method::DELETE, "/roles/web-editors", Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"], "role \"web-editors\" is in use by luis");
    assert_eq!(accounts_on_disk(&closed).role, accounts().role);

    // Free once nobody has it.
    let (status, _) = with_token(admin, Method::PUT, "/users/luis", json!({ "roles": [] })).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = with_token(admin, Method::DELETE, "/roles/web-editors", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_role_change_applies_to_a_signed_in_user_at_once() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let cookie = cookie_of(admin, "luis", "password-luis").await;
    let session = async || {
        let (_, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
        json
    };
    assert_eq!(
        session().await["grants"],
        json!([{ "group": "web", "allow": ["view", "create"] }])
    );

    // No wait: the next request sees the change.
    let api = json!({ "grant": [{ "group": "api", "allow": ["view"] }] });
    let (status, _) = with_token(admin, Method::PUT, "/roles/web-editors", api).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let changed = session().await;
    assert_eq!(
        changed["grants"],
        json!([{ "group": "api", "allow": ["view"] }])
    );
    assert_eq!(changed["creatable_groups"], json!([]));

    let (status, _, _) = call_with_cookie(admin, &cookie, Method::GET, "/users", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let promoted = json!({ "roles": ["admin"] });
    let (status, _) = with_token(admin, Method::PUT, "/users/luis", promoted).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/users", "").await;
    assert_eq!(status, StatusCode::OK, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn user_and_role_names_are_percent_decoded_and_validated() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;

    let eva = json!({ "name": "eva@example.com", "password": "password-eva", "roles": [] });
    let (status, json) = with_token(admin, Method::POST, "/users", eva).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let editor = json!({ "roles": ["web-editors"] });
    let (status, json) = with_token(admin, Method::PUT, "/users/eva%40example.com", editor).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(
        users_on_disk(&closed)[2],
        ("eva@example.com".to_owned(), names(&["web-editors"]))
    );
    let (status, json) = with_token(
        admin,
        Method::DELETE,
        "/users/eva%40example.com",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(users_on_disk(&closed).len(), 2);

    let (status, json) =
        with_token(admin, Method::POST, "/roles", json!({ "name": "web_ops" })).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let (status, json) = with_token(admin, Method::PUT, "/roles/web%5Fops", json!({})).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    let (status, json) = with_token(admin, Method::DELETE, "/roles/web%5fops", Value::Null).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");

    // A slash, encoded, is part of the name, which then names nobody.
    let (status, json) = with_token(admin, Method::DELETE, "/users/a%2Fb", Value::Null).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"], "no user \"a/b\"");

    // Invalid names are refused with the rule they break.
    let bad = json!({ "name": "Eva Example", "password": "password-eva", "roles": [] });
    let (status, json) = with_token(admin, Method::POST, "/users", bad).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        json["error"].as_str().expect("text").contains("a name is"),
        "{json}"
    );
    let (status, json) =
        with_token(admin, Method::POST, "/roles", json!({ "name": "web ops" })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        json["error"].as_str().expect("text").contains("a name is"),
        "{json}"
    );
    assert_eq!(users_on_disk(&closed).len(), 2);
    assert_eq!(accounts_on_disk(&closed).role, accounts().role);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_added_by_editing_the_file_can_sign_in_without_a_restart() {
    let closed = daemon(Some(&accounts()), /* token= */ true).await;
    let admin = closed.admin;
    let mut edited = accounts();
    edited
        .user
        .push(user("eva", "password-eva", &["web-editors"]));
    edited.save(&closed.users).expect("save");

    util::eventually(async || {
        let (_, json) = with_token(admin, Method::GET, "/users", Value::Null).await;
        json.as_array().is_some_and(|users| users.len() == 3)
    })
    .await;
    let cookie = cookie_of(admin, "eva", "password-eva").await;
    let (_, _, json) = call_with_cookie(admin, &cookie, Method::GET, "/auth/session", "").await;
    assert_eq!(json["user"], "eva");
    assert_eq!(json["roles"], json!(["web-editors"]));
}
