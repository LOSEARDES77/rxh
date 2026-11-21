//! Sign-in through an OpenID Connect provider: the flow against a fake
//! provider on localhost, and each way it is refused.

mod util;

use std::fs;
use std::net::SocketAddr;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::{HeaderMap, Method, Request, StatusCode, header};
use rxh_server::access::{Access, Grant, Permission, Role, Target, User};
use rxh_test_util::{OidcFault, OidcProvider, free_address};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use util::admin::{call, call_with_cookie, get, send, sign_in, start};

const POLICY: &str = "default-src 'none'";

/// A running daemon whose `[admin.oidc]` points at a fake provider.
struct Fixture {
    admin: SocketAddr,
    provider: OidcProvider,
    _directory: TempDir,
    _daemon: JoinHandle<Result<(), rxh_server::Error>>,
}

/// What a browser gets for a `GET`.
struct Page {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Page {
    /// The `name=value` a `Set-Cookie` of this answer gives to `name`.
    fn cookie(&self, name: &str) -> Option<String> {
        self.set_cookie(name)
            .map(|cookie| cookie.split(';').next().expect("a pair").to_owned())
    }

    /// The whole `Set-Cookie` of this answer for `name`.
    fn set_cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().expect("text").to_owned())
            .find(|cookie| cookie.starts_with(&format!("{name}=")))
    }

    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().expect("text")
    }
}

/// `ana`, a local administrator, and the role `web-editors`, which views
/// the hosts of the group `web`.
fn accounts() -> Access {
    Access {
        user: vec![User {
            name: "ana".to_owned(),
            password: bcrypt::hash("password-ana", 4).expect("hash"),
            roles: vec!["admin".to_owned()],
        }],
        role: vec![web_editors()],
    }
}

fn web_editors() -> Role {
    Role {
        name: "web-editors".to_owned(),
        grant: vec![Grant {
            target: Target::Group("web".to_owned()),
            allow: vec![Permission::View],
        }],
    }
}

/// Starts a provider and a daemon with the host `shop` of the group `web`
/// and the host `intra` of the group `internal`. The groups `ops`, `web`
/// and `ghosts` of the provider give the roles `admin`, `web-editors` and
/// one that is not defined.
async fn daemon() -> Fixture {
    daemon_of(OidcProvider::start().await, "").await
}

/// [`daemon`] for `provider`, with `keys` at the end of `[admin.oidc]`.
async fn daemon_of(provider: OidcProvider, keys: &str) -> Fixture {
    let oidc = format!(
        "public_url = \"http://ADMIN\"\n\n\
         [admin.oidc]\nissuer = \"{}\"\nclient_id = \"{}\"\nclient_secret = \"{}\"\n\
         roles = {{ ops = [\"admin\"], web = [\"web-editors\"], ghosts = [\"no-such-role\"] }}\n\
         {keys}",
        provider.issuer(),
        OidcProvider::CLIENT_ID,
        OidcProvider::CLIENT_SECRET,
    );
    let (admin, directory, daemon) = start_daemon(&oidc).await;
    Fixture {
        admin,
        provider,
        _directory: directory,
        _daemon: daemon,
    }
}

/// Starts a daemon with `admin_keys` (`ADMIN` stands for its address) at
/// the end of its `[admin]` table.
async fn start_daemon(
    admin_keys: &str,
) -> (
    SocketAddr,
    TempDir,
    JoinHandle<Result<(), rxh_server::Error>>,
) {
    let admin = free_address();
    let (shop, intra) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temporary directory");
    let users = directory.path().join("users.toml");
    accounts().save(&users).expect("write the users file");
    let config_path = directory.path().join("rxh.toml");
    let admin_keys = admin_keys.replace("ADMIN", &admin.to_string());
    fs::write(
        &config_path,
        format!(
            "[[server]]\nname = \"shop\"\nlisten = \"{shop}\"\ngroups = [\"web\"]\n\
             forward = \"127.0.0.1:9\"\n\n\
             [[server]]\nname = \"intra\"\nlisten = \"{intra}\"\ngroups = [\"internal\"]\n\
             forward = \"127.0.0.1:9\"\n\n\
             [admin]\nlisten = \"{admin}\"\nusers = {users:?}\n{admin_keys}"
        ),
    )
    .expect("write the configuration");
    let daemon = start(&config_path, admin).await;
    (admin, directory, daemon)
}

/// `GET uri` as a browser that holds `cookies` (`name=value` each).
async fn browse(admin: SocketAddr, uri: &str, cookies: &[&str]) -> Page {
    browse_as(admin, &admin.to_string(), uri, cookies).await
}

/// [`browse`] with another `Host`.
async fn browse_as(admin: SocketAddr, host: &str, uri: &str, cookies: &[&str]) -> Page {
    let mut request = Request::get(uri).header(header::HOST, host);
    if !cookies.is_empty() {
        request = request.header(header::COOKIE, cookies.join("; "));
    }
    let request = request.body(Empty::<Bytes>::new()).expect("valid request");
    let (status, headers, body) = util::send(admin, request).await;
    Page {
        status,
        headers,
        body: String::from_utf8(body.to_vec()).expect("text"),
    }
}

/// The value of `name` in the query of `url`, as it is written there.
fn query_value<'a>(url: &'a str, name: &str) -> &'a str {
    let (_, query) = url.split_once('?').expect("a query");
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .unwrap_or_else(|| panic!("no {name} in {url}"))
}

impl Fixture {
    /// Starts a flow: the answer of `GET /auth/oidc/login` and the binding
    /// cookie it set.
    async fn login(&self) -> (Page, String) {
        let page = browse(self.admin, "/auth/oidc/login", &[]).await;
        assert_eq!(page.status, StatusCode::FOUND, "{}", page.body);
        let binding = page.cookie("rxh_oidc").expect("the binding cookie");
        (page, binding)
    }

    /// The path and query of a callback URL the provider sends to.
    fn path<'a>(&self, callback: &'a str) -> &'a str {
        callback
            .strip_prefix(&format!("http://{}", self.admin))
            .unwrap_or_else(|| panic!("{callback} is not on public_url"))
    }

    /// The whole flow in one browser, up to the answer of the callback.
    async fn flow(&self) -> Page {
        let (login, binding) = self.login().await;
        let callback = self.provider.authorize(login.location());
        browse(self.admin, self.path(&callback), &[&binding]).await
    }

    /// The whole flow of a user whose token carries `claims`: the session
    /// cookie.
    async fn sign_in(&self, claims: Value) -> String {
        self.provider.set_claims(claims);
        let page = self.flow().await;
        assert_eq!(page.status, StatusCode::OK, "{}", page.body);
        page.cookie("rxh_session").expect("the session cookie")
    }

    /// `GET /auth/session` with a cookie.
    async fn session(&self, cookie: &str) -> Value {
        let headers = [(header::COOKIE, cookie)];
        let (status, _, json) = call(self.admin, Method::GET, "/auth/session", "", &headers).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json
    }

    /// Names of the hosts `GET /config/files` lists for a session.
    async fn hosts(&self, cookie: &str) -> Vec<String> {
        let headers = [(header::COOKIE, cookie)];
        let (status, _, json) = call(self.admin, Method::GET, "/config/files", "", &headers).await;
        assert_eq!(status, StatusCode::OK, "{json}");
        json["entities"]
            .as_array()
            .expect("entities")
            .iter()
            .filter(|entity| entity["kind"] == "server")
            .map(|entity| entity["value"]["name"].as_str().expect("name").to_owned())
            .collect()
    }

    /// Checks that `page` refuses a sign-in with `status` and a page that
    /// says `reason`, starts no session and holds nothing secret.
    fn assert_refused(&self, page: &Page, status: StatusCode, reason: &str) {
        assert_eq!(page.status, status, "{}", page.body);
        assert_eq!(
            page.headers[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(page.headers[header::CONTENT_SECURITY_POLICY], POLICY);
        assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
        assert!(page.body.contains(reason), "{reason:?} in {}", page.body);
        assert!(
            page.body
                .contains(&format!("href=\"http://{}/dashboard\"", self.admin)),
            "{}",
            page.body
        );
        assert_eq!(page.cookie("rxh_session"), None, "no session");
        self.assert_no_secret(&page.body);
    }

    /// Checks that `text` holds neither the client secret nor a code or a
    /// token of the provider.
    fn assert_no_secret(&self, text: &str) {
        let mut secrets = self.provider.secrets();
        secrets.push(OidcProvider::CLIENT_SECRET.to_owned());
        for secret in secrets {
            assert!(!text.contains(&secret), "{secret} in {text}");
        }
    }
}

fn marta() -> Value {
    json!({ "preferred_username": "marta", "groups": ["web"] })
}

#[tokio::test(flavor = "multi_thread")]
async fn the_flow_signs_a_mapped_user_in() {
    let fixture = daemon().await;
    let admin = fixture.admin;
    fixture
        .provider
        .set_claims(json!({ "preferred_username": "olga", "groups": ["ops", "web"] }));

    // The page learns that the provider is there, and under which name.
    let (_, json) = get(admin, "/auth/session").await;
    assert_eq!(json["oidc"], true);
    assert_eq!(json["oidc_issuer"], "127.0.0.1");
    assert_eq!(json["local"], false);

    let (login, binding) = fixture.login().await;
    let location = login.location();
    let endpoint = format!("{}/authorize?", fixture.provider.issuer());
    assert!(location.starts_with(&endpoint), "{location}");
    assert_eq!(query_value(location, "response_type"), "code");
    assert_eq!(query_value(location, "client_id"), OidcProvider::CLIENT_ID);
    assert_eq!(
        query_value(location, "redirect_uri"),
        format!(
            "http%3A%2F%2F{}%3A{}%2Fauth%2Foidc%2Fcallback",
            admin.ip(),
            admin.port()
        )
    );
    assert_eq!(query_value(location, "scope"), "openid%20profile%20email");
    assert_eq!(query_value(location, "code_challenge_method"), "S256");
    for random in ["state", "nonce", "code_challenge"] {
        assert_eq!(query_value(location, random).len(), 43, "{random}");
    }
    assert_eq!(
        login.set_cookie("rxh_oidc").expect("set"),
        format!("{binding}; HttpOnly; SameSite=Lax; Path=/; Max-Age=600")
    );
    assert_eq!(login.headers[header::CACHE_CONTROL], "no-store");

    // The provider checks the secret, the redirect URI, the verifier
    // against the challenge, and gives the code once.
    let callback = fixture.provider.authorize(location);
    let page = browse(admin, fixture.path(&callback), &[&binding]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(fixture.provider.token_requests(), 1);
    assert_eq!(
        page.headers[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert_eq!(page.headers[header::CONTENT_SECURITY_POLICY], POLICY);
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(page.headers[header::REFERRER_POLICY], "no-referrer");
    let dashboard = format!("http://{admin}/dashboard");
    assert!(
        page.body.contains(&format!(
            "<meta http-equiv=\"refresh\" content=\"0; url={dashboard}\">"
        )),
        "{}",
        page.body
    );
    assert!(
        page.body.contains(&format!("href=\"{dashboard}\"")),
        "{}",
        page.body
    );
    assert!(!page.body.contains("<script"), "{}", page.body);
    let session = page.set_cookie("rxh_session").expect("the session cookie");
    assert!(
        session.ends_with("; HttpOnly; SameSite=Strict; Path=/"),
        "{session}"
    );
    assert_eq!(
        page.set_cookie("rxh_oidc").expect("cleared"),
        "rxh_oidc=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    );

    let cookie = page.cookie("rxh_session").expect("the session cookie");
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "olga");
    assert_eq!(json["admin"], true);
    assert_eq!(json["roles"], json!(["admin", "web-editors"]));
    assert_eq!(json["local"], false);
    // An administrator's route answers it.
    let headers = [(header::COOKIE, cookie.as_str())];
    let (status, _, json) = call(admin, Method::GET, "/users", "", &headers).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // A local user is local, and signing out ends the session.
    let (_, headers, _) = sign_in(admin, "ana", "password-ana").await;
    let ana = util::admin::session_cookie(&headers);
    assert_eq!(fixture.session(&ana).await["local"], true);
    let (status, _, _) = call_with_cookie(admin, &cookie, Method::POST, "/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(fixture.session(&cookie).await["user"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_roles_come_from_the_groups_claim() {
    let fixture = daemon().await;
    // A group that is not mapped adds nothing.
    let cookie = fixture
        .sign_in(json!({ "preferred_username": "marta", "groups": ["web", "sales", 7] }))
        .await;
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "marta");
    assert_eq!(json["admin"], false);
    assert_eq!(json["roles"], json!(["web-editors"]));
    assert_eq!(fixture.hosts(&cookie).await, ["shop"]);
    let headers = [(header::COOKIE, cookie.as_str())];
    let (status, _, json) = call(fixture.admin, Method::GET, "/users", "", &headers).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");

    // One group as a string, and a name claim that is missing: `sub`.
    let cookie = fixture.sign_in(json!({ "groups": "web" })).await;
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "subject-1");
    assert_eq!(json["roles"], json!(["web-editors"]));

    // A group mapped to a role that is not defined signs in to nothing.
    let cookie = fixture
        .sign_in(json!({ "preferred_username": "gus", "groups": ["ghosts"] }))
        .await;
    let json = fixture.session(&cookie).await;
    assert_eq!(json["roles"], json!(["no-such-role"]));
    assert_eq!(json["grants"], json!([]));
    assert_eq!(fixture.hosts(&cookie).await, [] as [&str; 0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_without_a_mapped_role_is_refused_and_named() {
    let fixture = daemon().await;
    fixture
        .provider
        .set_claims(json!({ "preferred_username": "pat", "groups": ["sales", "staff"] }));
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "pat");
    assert!(page.body.contains("sales, staff"), "{}", page.body);
    assert_eq!(
        page.set_cookie("rxh_oidc").expect("cleared"),
        "rxh_oidc=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"
    );

    // No groups claim at all.
    fixture
        .provider
        .set_claims(json!({ "preferred_username": "pat" }));
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "pat");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_with_markup_is_shown_as_text() {
    let fixture = daemon().await;
    fixture.provider.set_claims(json!({
        "preferred_username": "<script>alert('pat')</script>",
        "groups": ["\"><img src=x>"],
    }));
    let page = fixture.flow().await;
    fixture.assert_refused(
        &page,
        StatusCode::FORBIDDEN,
        "&lt;script&gt;alert(&#39;pat&#39;)&lt;/script&gt;",
    );
    assert!(page.body.contains("&quot;&gt;&lt;img src=x&gt;"));
    assert!(!page.body.contains("<script") && !page.body.contains("<img"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_without_the_binding_cookie_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    let (login, binding) = fixture.login().await;
    let callback = fixture.provider.authorize(login.location());
    let path = fixture.path(&callback);

    let page = browse(fixture.admin, path, &[]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "not started in this browser");
    assert_eq!(
        fixture.provider.token_requests(),
        0,
        "the code was not sent"
    );

    // The flow is spent: the browser that started it has to start again.
    let page = browse(fixture.admin, path, &[&binding]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "not started in this browser");
    assert_eq!(fixture.provider.token_requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_of_another_browser_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    // The attacker signs in at the provider and hands its callback to the
    // browser of somebody else, who has a flow of its own under way.
    let (attacker, _) = fixture.login().await;
    let (_, victim) = fixture.login().await;
    let callback = fixture.provider.authorize(attacker.location());

    let page = browse(fixture.admin, fixture.path(&callback), &[&victim]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "not started in this browser");
    assert_eq!(
        fixture.provider.token_requests(),
        0,
        "the code was not sent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_callback_cannot_be_used_twice() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    let (login, binding) = fixture.login().await;
    let callback = fixture.provider.authorize(login.location());
    let path = fixture.path(&callback);
    let page = browse(fixture.admin, path, &[&binding]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    let cookie = page.cookie("rxh_session").expect("the session cookie");

    let again = browse(fixture.admin, path, &[&binding]).await;
    fixture.assert_refused(&again, StatusCode::FORBIDDEN, "not started in this browser");
    assert_eq!(fixture.provider.token_requests(), 1);

    // The back button after signing out.
    let (status, _, _) =
        call_with_cookie(fixture.admin, &cookie, Method::POST, "/auth/logout", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let back = browse(fixture.admin, path, &[&binding, &cookie]).await;
    fixture.assert_refused(&back, StatusCode::FORBIDDEN, "not started in this browser");
    assert_eq!(fixture.session(&cookie).await["user"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_from_the_provider_is_shown_and_starts_no_session() {
    let fixture = daemon().await;
    let (login, binding) = fixture.login().await;
    let state = query_value(login.location(), "state");
    let path = format!(
        "/auth/oidc/callback?error=access_denied&error_description=%3Cb%3Eno%3C%2Fb%3E&state={state}"
    );
    let page = browse(fixture.admin, &path, &[&binding]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "access_denied");
    assert!(!page.body.contains("<b>"), "{}", page.body);
    assert_eq!(fixture.provider.token_requests(), 0);

    // An error for a flow of nobody is not repeated.
    let path = "/auth/oidc/callback?error=made_up_text&state=unknown";
    let page = browse(fixture.admin, path, &[&binding]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "not started in this browser");
    assert!(!page.body.contains("made_up_text"), "{}", page.body);

    // Neither a code nor an error.
    let (login, binding) = fixture.login().await;
    let state = query_value(login.location(), "state");
    let path = format!("/auth/oidc/callback?state={state}");
    let page = browse(fixture.admin, &path, &[&binding]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "no code");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_without_an_id_token_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    fixture.provider.fail_with(OidcFault::NoIdToken);
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "no ID token");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_with_a_wrong_claim_is_refused() {
    let fixture = daemon().await;
    for (claims, reason) in [
        (json!({ "iss": "https://another-issuer.example" }), "issuer"),
        (json!({ "aud": "another-client" }), "audience"),
        (json!({ "nonce": "another-nonce" }), "nonce"),
        (json!({ "exp": 1_000_000_000 }), "expired"),
    ] {
        let mut claims = claims;
        claims["preferred_username"] = json!("marta");
        claims["groups"] = json!(["ops"]);
        fixture.provider.set_claims(claims);
        let page = fixture.flow().await;
        fixture.assert_refused(&page, StatusCode::FORBIDDEN, reason);
        assert!(!page.body.contains("marta"), "{}", page.body);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_error_of_the_token_endpoint_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    fixture.provider.set_token_error(500);
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "500");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_token_endpoint_is_refused_after_ten_seconds() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    fixture.provider.fail_with(OidcFault::Slow);
    let started = std::time::Instant::now();
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "did not answer in time");
    let waited = started.elapsed().as_secs();
    assert!((9..20).contains(&waited), "{waited} seconds");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_huge_answer_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    fixture.provider.fail_with(OidcFault::Huge);
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "too long");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_document_of_another_issuer_is_refused() {
    let fixture = daemon().await;
    fixture.provider.fail_with(OidcFault::OtherIssuer);
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "another issuer");
    assert_eq!(page.cookie("rxh_oidc"), None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_discovery_document_with_an_http_endpoint_is_refused() {
    let fixture = daemon().await;
    fixture.provider.fail_with(OidcFault::HttpEndpoint);
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "not https");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_that_is_down_is_a_bad_gateway() {
    let issuer = format!("http://{}", util::dead_address());
    let (admin, _directory, _daemon) = start_daemon(&format!(
        "public_url = \"http://ADMIN\"\n\n\
         [admin.oidc]\nissuer = \"{issuer}\"\nclient_id = \"rxh\"\nclient_secret = \"a-secret\"\n"
    ))
    .await;
    let page = browse(admin, "/auth/oidc/login", &[]).await;
    assert_eq!(page.status, StatusCode::BAD_GATEWAY, "{}", page.body);
    assert!(page.body.contains("could not be reached"), "{}", page.body);
    // What the system said about it is for the log, not for anybody.
    for detail in ["os error", "refused", "Refused", "("] {
        assert!(!page.body.contains(detail), "{detail} in {}", page.body);
    }
    assert!(!page.body.contains("a-secret"), "{}", page.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn login_is_404_without_oidc() {
    let (admin, _directory, _daemon) = start_daemon("").await;
    for uri in ["/auth/oidc/login", "/auth/oidc/callback?code=a&state=b"] {
        let page = browse(admin, uri, &[]).await;
        assert_eq!(page.status, StatusCode::NOT_FOUND, "{uri}: {}", page.body);
        assert_eq!(page.headers.get(header::SET_COOKIE), None, "{uri}");
    }
    let (_, json) = get(admin, "/auth/session").await;
    assert_eq!(json["oidc"], false);
    assert_eq!(json["oidc_issuer"], Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_password_route_refuses_an_oidc_session() {
    let fixture = daemon().await;
    let cookie = fixture.sign_in(marta()).await;
    let body = json!({ "current": "password-marta", "new": "another-password" }).to_string();
    let (status, _, json) =
        call_with_cookie(fixture.admin, &cookie, Method::PUT, "/auth/password", &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        json,
        json!({ "error": "this account signs in through the provider" })
    );
    assert_eq!(fixture.session(&cookie).await["user"], "marta");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oidc_user_named_like_a_local_administrator_gets_only_its_mapped_roles() {
    let fixture = daemon().await;
    let admin = fixture.admin;
    // The provider knows an `ana` too, who is not the local administrator.
    let cookie = fixture
        .sign_in(json!({ "preferred_username": "ana", "groups": ["web"] }))
        .await;
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "ana");
    assert_eq!(json["admin"], false);
    assert_eq!(json["roles"], json!(["web-editors"]));
    assert_eq!(json["local"], false);
    let headers = [(header::COOKIE, cookie.as_str())];
    let (status, _, json) = call(admin, Method::GET, "/users", "", &headers).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.hosts(&cookie).await, ["shop"]);

    // Not even with the password of the local `ana` does it change it.
    let body = json!({ "current": "password-ana", "new": "another-password" }).to_string();
    let (status, _, json) =
        call_with_cookie(admin, &cookie, Method::PUT, "/auth/password", &body).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    let (status, headers, _) = sign_in(admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "the password is the same");
    let local = util::admin::session_cookie(&headers);
    assert_eq!(fixture.session(&local).await["admin"], true);

    // What ends the sessions of the local `ana` leaves the other one.
    let body = json!({ "password": "a-new-password" }).to_string();
    let (status, json) = send(admin, Method::PUT, "/users/ana", &body).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(fixture.session(&local).await["user"], Value::Null);
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "ana");
    assert_eq!(json["roles"], json!(["web-editors"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_role_takes_the_access_at_once() {
    let fixture = daemon().await;
    let cookie = fixture.sign_in(marta()).await;
    assert_eq!(fixture.hosts(&cookie).await, ["shop"]);

    let (status, json) = send(fixture.admin, Method::DELETE, "/roles/web-editors", "").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{json}");
    assert_eq!(fixture.hosts(&cookie).await, [] as [&str; 0]);
    let json = fixture.session(&cookie).await;
    assert_eq!(json["user"], "marta", "still signed in");
    assert_eq!(json["grants"], json!([]));

    // The membership is the one of sign-in time: the role, defined again
    // with more in it, is the user's again.
    let mut role = web_editors();
    role.grant[0].target = Target::Group("internal".to_owned());
    let body = serde_json::to_string(&role).expect("JSON");
    let (status, json) = send(fixture.admin, Method::POST, "/roles", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(fixture.hosts(&cookie).await, ["intra"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn starting_a_flow_shares_the_attempt_limit_with_sign_in() {
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    let (login, binding) = fixture.login().await;
    for _ in 0..9 {
        fixture.login().await;
    }
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(
        &page,
        StatusCode::TOO_MANY_REQUESTS,
        "too many sign-in attempts",
    );
    assert_eq!(page.cookie("rxh_oidc"), None);
    let (status, _, json) = sign_in(fixture.admin, "ana", "password-ana").await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{json}");

    // The callback is not an attempt: a flow that was started ends, and
    // one that was not does nothing.
    let page = browse(fixture.admin, "/auth/oidc/callback?code=a&state=b", &[]).await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "not started in this browser");
    let callback = fixture.provider.authorize(login.location());
    let page = browse(fixture.admin, fixture.path(&callback), &[&binding]).await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_discovery_document_is_read_once_and_a_failure_is_not_kept() {
    let fixture = daemon().await;
    fixture.provider.fail_with(OidcFault::OtherIssuer);
    for requests in 1..=2 {
        let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
        assert_eq!(page.status, StatusCode::BAD_GATEWAY, "{}", page.body);
        assert_eq!(fixture.provider.discovery_requests(), requests);
    }
    fixture.provider.recover();
    fixture.login().await;
    assert_eq!(fixture.provider.discovery_requests(), 3);

    // A good one is kept: neither a login nor a callback reads it again.
    fixture.provider.set_claims(marta());
    let page = fixture.flow().await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    fixture.login().await;
    assert_eq!(fixture.provider.discovery_requests(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_redirect_from_the_provider_is_not_followed() {
    // Where the provider sends: another provider that would answer well.
    let elsewhere = OidcProvider::start().await;
    let address: SocketAddr = elsewhere.issuer()["http://".len()..]
        .parse()
        .expect("an address");

    // From the token endpoint, with the code and the client secret.
    let fixture = daemon().await;
    fixture.provider.set_claims(marta());
    let (login, binding) = fixture.login().await;
    let callback = fixture.provider.authorize(login.location());
    fixture.provider.fail_with(OidcFault::Redirect(address));
    let page = browse(fixture.admin, fixture.path(&callback), &[&binding]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "302");
    assert_eq!(fixture.provider.token_requests(), 1);
    assert_eq!(elsewhere.token_requests(), 0);

    // From the discovery document.
    let fixture = daemon().await;
    fixture.provider.fail_with(OidcFault::Redirect(address));
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "302");
    assert_eq!(fixture.provider.discovery_requests(), 1);
    assert_eq!(elsewhere.discovery_requests(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_name_with_control_characters_is_refused() {
    let fixture = daemon().await;
    fixture.provider.set_claims(json!({
        "preferred_username": "eve\nAdmin => ana signed in from 192.0.2.1",
        "groups": ["ops"],
    }));
    let page = fixture.flow().await;
    fixture.assert_refused(&page, StatusCode::FORBIDDEN, "control characters");
    assert!(!page.body.contains("eve"), "{}", page.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_redirect_uri_comes_from_public_url_and_not_from_host() {
    let fixture = daemon().await;
    let page = browse_as(fixture.admin, "localhost:1", "/auth/oidc/login", &[]).await;
    assert_eq!(page.status, StatusCode::FOUND, "{}", page.body);
    assert_eq!(
        query_value(page.location(), "redirect_uri"),
        format!(
            "http%3A%2F%2F{}%3A{}%2Fauth%2Foidc%2Fcallback",
            fixture.admin.ip(),
            fixture.admin.port()
        )
    );
    assert!(
        !page.location().contains("localhost"),
        "{}",
        page.location()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_provider_behind_a_private_authority_needs_ca() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let ca = directory.path().join("provider-ca.pem");

    // With `ca`, the whole flow over HTTPS.
    let provider = OidcProvider::start_tls().await;
    assert!(provider.issuer().starts_with("https://127.0.0.1:"));
    fs::write(&ca, provider.authority_pem().expect("over TLS")).expect("write the authority");
    let fixture = daemon_of(provider, &format!("ca = {ca:?}\n")).await;
    let cookie = fixture.sign_in(marta()).await;
    assert_eq!(fixture.session(&cookie).await["user"], "marta");
    assert_eq!(fixture.provider.token_requests(), 1);

    // Without it the certificate is of nobody the daemon trusts.
    let fixture = daemon_of(OidcProvider::start_tls().await, "").await;
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "could not be reached");
    assert_eq!(fixture.provider.discovery_requests(), 0);

    // Nor with the authority of another provider.
    let fixture = daemon_of(OidcProvider::start_tls().await, &format!("ca = {ca:?}\n")).await;
    let page = browse(fixture.admin, "/auth/oidc/login", &[]).await;
    fixture.assert_refused(&page, StatusCode::BAD_GATEWAY, "could not be reached");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_secret_appears_in_pages_or_config() {
    let fixture = daemon().await;
    let admin = fixture.admin;
    fixture.provider.set_claims(marta());
    let (login, binding) = fixture.login().await;
    let location = login.location().to_owned();
    let callback = fixture.provider.authorize(&location);
    let path = fixture.path(&callback);
    let signed_in = browse(admin, path, &[&binding]).await;
    assert_eq!(signed_in.status, StatusCode::OK);
    let cookie = signed_in.cookie("rxh_session").expect("the session cookie");
    let replayed = browse(admin, path, &[&binding]).await;
    fixture.provider.set_claims(json!({ "groups": [] }));
    let refused = fixture.flow().await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let (_, config) = get(admin, "/config").await;
    assert_eq!(config["admin"]["oidc"]["client_secret"], "***");
    let (_, files) = get(admin, "/config/files").await;
    let session = fixture.session(&cookie).await;
    assert_eq!(fixture.provider.secrets().len(), 6, "codes and tokens");
    let state = query_value(&location, "state");
    for body in [
        login.body.clone(),
        signed_in.body.clone(),
        replayed.body.clone(),
        refused.body.clone(),
        config.to_string(),
        files.to_string(),
        session.to_string(),
    ] {
        fixture.assert_no_secret(&body);
        assert!(!body.contains(state), "the state in {body}");
        assert!(!body.contains(query_value(&callback, "code")), "{body}");
    }
    // Only the callback carries the code, and no answer sends it on.
    for page in [&signed_in, &replayed, &refused] {
        assert_eq!(page.headers.get(header::LOCATION), None);
    }
}
