//! Grants on hosts: what a user without the `admin` role sees and changes
//! through the editing and the statistics routes of the admin API.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use hyper::{Method, StatusCode, header};
use rxh_server::access::{Access, Grant, Permission, Role, Target, User};
use rxh_test_util::free_address;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use util::admin::{call, call_with_cookie, session_cookie, sign_in, start};

const TOKEN: &str = "s3cret-admin-token-4567";
const NEEDS_ADMIN: &str = "this needs the admin role";
const STALE: &str = "the configuration changed, read it again";

/// Keys of the servers of the fixture.
const SHOP: &str = "0:0";
const BLOG: &str = "0:1";
const INTRA: &str = "0:2";
const UNNAMED: &str = "0:3";

/// A running daemon with four hosts and the users of [`accounts`].
struct Fixture {
    admin: SocketAddr,
    config_path: PathBuf,
    /// Listen addresses of `shop`, `blog`, `intra` and the host without a
    /// name.
    listens: [SocketAddr; 4],
    _directory: TempDir,
    _daemon: JoinHandle<Result<(), rxh_server::Error>>,
}

impl Fixture {
    fn text(&self) -> String {
        fs::read_to_string(&self.config_path).expect("read the configuration")
    }

    /// The session cookie of a user, whose password is `password-NAME`.
    async fn cookie(&self, name: &str) -> String {
        let (status, headers, json) = sign_in(self.admin, name, &format!("password-{name}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{name}: {json}");
        session_cookie(&headers)
    }

    /// A request with the session cookie of a user.
    async fn user(
        &self,
        cookie: &str,
        method: Method,
        uri: &str,
        body: &Value,
    ) -> (StatusCode, Value) {
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let (status, _, json) = call_with_cookie(self.admin, cookie, method, uri, &body).await;
        (status, json)
    }

    async fn get(&self, cookie: &str, uri: &str) -> Value {
        let (status, json) = self.user(cookie, Method::GET, uri, &Value::Null).await;
        assert_eq!(status, StatusCode::OK, "GET {uri}: {json}");
        json
    }

    /// A request with the token.
    async fn token(&self, method: Method, uri: &str, body: &Value) -> (StatusCode, Value) {
        let bearer = format!("Bearer {TOKEN}");
        let headers = [(header::AUTHORIZATION, bearer.as_str())];
        let body = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let (status, _, json) = call(self.admin, method, uri, &body, &headers).await;
        (status, json)
    }

    /// Revision of the main file, as the token reads it.
    async fn revision(&self) -> String {
        let (_, files) = self.token(Method::GET, "/config/files", &Value::Null).await;
        files["files"][0]["revision"]
            .as_str()
            .expect("revision")
            .to_owned()
    }

    /// The form value of a server, as the token reads it.
    async fn value(&self, key: &str) -> Value {
        let (_, files) = self.token(Method::GET, "/config/files", &Value::Null).await;
        server(&files, key).expect("the server is listed")["value"].clone()
    }

    /// `PUT` of a server with the current revision.
    async fn put(&self, cookie: &str, key: &str, content: Value) -> (StatusCode, Value) {
        let mut body = content;
        body["revision"] = json!(self.revision().await);
        let uri = format!("/config/entities/server/{key}");
        self.user(cookie, Method::PUT, &uri, &body).await
    }

    /// `PUT` of the form value of a server with `change` applied to it.
    async fn put_value(
        &self,
        cookie: &str,
        key: &str,
        change: impl FnOnce(&mut Value),
    ) -> (StatusCode, Value) {
        let mut value = self.value(key).await;
        change(&mut value);
        self.put(cookie, key, json!({ "value": value })).await
    }

    /// `DELETE` of a server with the current revision.
    async fn delete(&self, cookie: &str, key: &str) -> (StatusCode, Value) {
        let uri = format!(
            "/config/entities/server/{key}?revision={}",
            self.revision().await
        );
        self.user(cookie, Method::DELETE, &uri, &Value::Null).await
    }

    /// `POST` of a new server.
    async fn create(&self, cookie: &str, value: Value) -> (StatusCode, Value) {
        let body = json!({ "value": value });
        self.user(cookie, Method::POST, "/config/entities/server", &body)
            .await
    }
}

/// A user whose hash has the lowest cost, so that signing in is fast.
fn user(name: &str, roles: &[&str]) -> User {
    User {
        name: name.to_owned(),
        password: bcrypt::hash(format!("password-{name}"), 4).expect("hash"),
        roles: roles.iter().map(|role| (*role).to_owned()).collect(),
    }
}

fn role(name: &str, target: Target, allow: &[Permission]) -> Role {
    Role {
        name: name.to_owned(),
        grant: vec![Grant {
            target,
            allow: allow.to_vec(),
        }],
    }
}

/// `ana` the administrator, `vera` who views the group `web`, `eva` who
/// does everything in it, `sam` who views and changes the host `shop`,
/// `pia` who creates in the group `press`, and `noa` who is `eva` and `pia`.
fn accounts() -> Access {
    use Permission::{Create, Delete, Modify, View};

    let group = |name: &str| Target::Group(name.to_owned());
    Access {
        user: vec![
            user("ana", &["admin"]),
            user("vera", &["web-view"]),
            user("eva", &["web-edit"]),
            user("sam", &["shop-only"]),
            user("pia", &["press-create"]),
            user("noa", &["web-edit", "press-create"]),
        ],
        role: vec![
            role("web-view", group("web"), &[View]),
            role("web-edit", group("web"), &[View, Modify, Create, Delete]),
            role(
                "shop-only",
                Target::Host("shop".to_owned()),
                &[View, Modify],
            ),
            role("press-create", group("press"), &[Create]),
        ],
    }
}

async fn daemon() -> Fixture {
    let admin = free_address();
    let listens = [
        free_address(),
        free_address(),
        free_address(),
        free_address(),
    ];
    let directory = tempfile::tempdir().expect("temporary directory");
    let users = directory.path().join("users.toml");
    accounts().save(&users).expect("write the users file");
    let config_path = directory.path().join("rxh.toml");
    let [shop, blog, intra, unnamed] = listens;
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\nusers = {users:?}\ntoken = \"{TOKEN}\"\n\n\
             # The pool of the shop. This comment stays.\n\
             [upstream.api]\nbackends = [\"127.0.0.1:9\"]\n\n\
             # The shop. This comment stays.\n\
             [[server]]\nname = \"shop\"\nlisten = \"{shop}\"\ngroups = [\"web\"]\n\
             forward = \"@api\" # To the pool.\n\n\
             [[server]]\nname = \"blog\"\nlisten = \"{blog}\"\ngroups = [\"web\", \"press\"]\n\
             forward = \"127.0.0.1:9\"\n\n\
             # Not for the web team.\n\
             [[server]]\nname = \"intra\"\nlisten = \"{intra}\"\ngroups = [\"internal\"]\n\
             forward = \"127.0.0.1:9\"\n\n\
             [[server]]\nlisten = \"{unnamed}\"\ngroups = [\"web\"]\nforward = \"127.0.0.1:9\"\n"
        ),
    )
    .expect("write the configuration");
    let daemon = start(&config_path, admin).await;
    Fixture {
        admin,
        config_path,
        listens,
        _directory: directory,
        _daemon: daemon,
    }
}

/// The server entity with `key` of a `GET /config/files` answer.
fn server<'a>(files: &'a Value, key: &str) -> Option<&'a Value> {
    files["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .find(|entity| entity["kind"] == "server" && entity["key"] == key)
}

/// Keys of the entities of `kind` of a `GET /config/files` answer.
fn keys(files: &Value, kind: &str) -> Vec<String> {
    files["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .filter(|entity| entity["kind"] == kind)
        .map(|entity| entity["key"].as_str().expect("key").to_owned())
        .collect()
}

/// A host for `POST /config/entities/server`.
fn new_host(groups: Option<&[&str]>) -> Value {
    let mut value = json!({ "listen": free_address().to_string(), "forward": "127.0.0.1:9" });
    if let Some(groups) = groups {
        value["groups"] = json!(groups);
    }
    value
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_lists_only_its_hosts_and_no_file_text() {
    let fixture = daemon().await;
    let vera = fixture.cookie("vera").await;
    let files = fixture.get(&vera, "/config/files").await;

    assert_eq!(files["editable"], true);
    assert_eq!(
        files["files"],
        json!([{ "index": 0, "revision": fixture.revision().await }]),
        "a file is its index and its revision"
    );
    assert_eq!(keys(&files, "server"), [SHOP, BLOG, UNNAMED]);
    assert_eq!(keys(&files, "section"), [] as [&str; 0]);
    for key in [SHOP, BLOG, UNNAMED] {
        let host = server(&files, key).expect("listed");
        assert_eq!(host["permissions"], json!(["view"]), "{key}");
        assert_eq!(host["file"], 0);
    }
    let shop = server(&files, SHOP).expect("listed");
    assert_eq!(shop["value"]["name"], "shop");
    assert!(shop["toml"].as_str().expect("toml").contains("@api"));

    let text = files.to_string();
    for hidden in ["intra", "internal", TOKEN, "Not for the web team", "users"] {
        assert!(!text.contains(hidden), "{hidden} in {text}");
    }
    assert!(
        !text.contains(&fixture.listens[2].to_string()),
        "the address of intra"
    );

    // Somebody whose roles reach no host sees none.
    let pia = fixture.cookie("pia").await;
    let files = fixture.get(&pia, "/config/files").await;
    assert_eq!(keys(&files, "server"), [] as [&str; 0]);
    assert!(files["files"][0]["revision"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn pools_are_names_only_for_a_user() {
    let fixture = daemon().await;
    let vera = fixture.cookie("vera").await;
    let files = fixture.get(&vera, "/config/files").await;
    let pools: Vec<&Value> = files["entities"]
        .as_array()
        .expect("entities")
        .iter()
        .filter(|entity| entity["kind"] == "upstream")
        .collect();
    assert_eq!(pools, [&json!({ "kind": "upstream", "key": "api" })]);

    // The token still gets the pool whole.
    let (_, files) = fixture
        .token(Method::GET, "/config/files", &Value::Null)
        .await;
    let pool = &files["entities"][0];
    assert_eq!(pool["value"]["backends"][0], "127.0.0.1:9");
    assert!(pool.get("permissions").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_view_answers_like_a_missing_one() {
    let fixture = daemon().await;
    let eva = fixture.cookie("eva").await;
    let before = fixture.text();

    let unchanged = |value: &mut Value| value["max_connections"] = json!(3);
    let hidden = fixture.put_value(&eva, INTRA, unchanged).await;
    let value = fixture.value(INTRA).await;
    let missing = fixture.put(&eva, "0:9", json!({ "value": value })).await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND, "{}", hidden.1);
    assert_eq!(hidden, missing);
    for key in ["1:0", "0", "x:y", "0:01"] {
        let other = fixture.put(&eva, key, json!({ "value": value })).await;
        assert_eq!(other, missing, "{key}");
    }

    let hidden = fixture.delete(&eva, INTRA).await;
    let missing = fixture.delete(&eva, "0:9").await;
    assert_eq!(hidden.0, StatusCode::NOT_FOUND, "{}", hidden.1);
    assert_eq!(hidden, missing);

    // Create without View does not show the host either.
    let pia = fixture.cookie("pia").await;
    let value = fixture.value(BLOG).await;
    let hidden = fixture.put(&pia, BLOG, json!({ "value": value })).await;
    assert_eq!(hidden, missing);
    assert_eq!(fixture.text(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn modify_changes_a_host_and_the_file() {
    let fixture = daemon().await;
    let eva = fixture.cookie("eva").await;
    let (status, json) = fixture
        .put_value(&eva, SHOP, |value| value["max_connections"] = json!(7))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json, json!({ "saved": true }));

    let text = fixture.text();
    assert!(text.contains("max_connections = 7"), "{text}");
    for kept in [
        "# The pool of the shop. This comment stays.",
        "# The shop. This comment stays.",
        "# To the pool.",
        "# Not for the web team.",
    ] {
        assert!(text.contains(kept), "{kept} in {text}");
    }
    let (_, config) = fixture.token(Method::GET, "/config", &Value::Null).await;
    assert_eq!(config["server"][0]["max_connections"], 7, "{config}");
    assert_eq!(config["server"][2]["name"], "intra");

    // View alone does not change a host.
    let vera = fixture.cookie("vera").await;
    let (status, json) = fixture
        .put_value(&vera, SHOP, |value| value["max_connections"] = json!(8))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), text);
}

#[tokio::test(flavor = "multi_thread")]
async fn modify_through_the_toml_of_the_table_is_checked_the_same() {
    let fixture = daemon().await;
    let eva = fixture.cookie("eva").await;
    let listen = fixture.listens[0];
    let table = |name: &str, groups: &str| {
        format!(
            "name = \"{name}\"\nlisten = \"{listen}\"\ngroups = {groups}\n\
             forward = \"@api\"\nmax_connections = 9\n"
        )
    };

    let before = fixture.text();
    for (name, groups) in [
        ("store", "[\"web\"]"),
        ("shop", "[\"web\", \"press\"]"),
        ("shop", "[\"internal\"]"),
        ("shop", "[]"),
    ] {
        let toml = table(name, groups);
        let (status, json) = fixture.put(&eva, SHOP, json!({ "toml": toml })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name} {groups}: {json}");
        assert_eq!(fixture.text(), before, "{name} {groups}");
    }
    // Text that defines another table is not the table of the host.
    let escape = format!(
        "{}\n[[server]]\nlisten = \"{}\"\nforward = \"127.0.0.1:9\"\n",
        table("shop", "[\"web\"]"),
        free_address()
    );
    let (status, json) = fixture.put(&eva, SHOP, json!({ "toml": escape })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(fixture.text(), before);
    // Neither is a table whose `groups` is not a list.
    let toml = table("shop", "\"web\"");
    let (status, json) = fixture.put(&eva, SHOP, json!({ "toml": toml })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
    assert_eq!(fixture.text(), before);

    let toml = table("shop", "[\"web\"]");
    let (status, json) = fixture.put(&eva, SHOP, json!({ "toml": toml })).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let text = fixture.text();
    assert!(text.contains("max_connections = 9"), "{text}");
    assert!(text.contains("# The shop. This comment stays."), "{text}");
    assert!(text.contains("name = \"intra\""), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn adding_a_group_needs_create_on_it() {
    let fixture = daemon().await;
    let before = fixture.text();
    let add =
        |group: &'static str| move |value: &mut Value| value["groups"] = json!(["web", group]);

    let eva = fixture.cookie("eva").await;
    for group in ["press", "internal", "new"] {
        let (status, json) = fixture.put_value(&eva, SHOP, add(group)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{group}: {json}");
        assert_eq!(fixture.text(), before, "{group}");
    }

    let noa = fixture.cookie("noa").await;
    let (status, json) = fixture.put_value(&noa, SHOP, add("internal")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    let (status, json) = fixture.put_value(&noa, SHOP, add("press")).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(fixture.value(SHOP).await["groups"], json!(["web", "press"]));
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_the_group_that_grants_modify_is_refused() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    for groups in [json!(["press"]), json!([])] {
        let (status, json) = fixture
            .put_value(&eva, BLOG, |value| value["groups"] = groups.clone())
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{groups}: {json}");
    }
    let (status, json) = fixture
        .put_value(&eva, BLOG, |value| {
            value.as_object_mut().expect("object").remove("groups");
        })
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    // A group that grants nothing goes, and so does the only group of a
    // host its name still reaches.
    let (status, json) = fixture
        .put_value(&eva, BLOG, |value| value["groups"] = json!(["web"]))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let sam = fixture.cookie("sam").await;
    let (status, json) = fixture
        .put_value(&sam, SHOP, |value| value["groups"] = json!([]))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let files = fixture.get(&sam, "/config/files").await;
    assert_eq!(keys(&files, "server"), [SHOP]);
}

#[tokio::test(flavor = "multi_thread")]
async fn renaming_a_host_needs_admin() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    let rename = |value: &mut Value| value["name"] = json!("store");

    let (status, json) = fixture.put_value(&eva, SHOP, rename).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    let (status, json) = fixture.put_value(&eva, UNNAMED, rename).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a name for a host without: {json}"
    );
    let (status, json) = fixture
        .put_value(&eva, SHOP, |value| {
            value.as_object_mut().expect("object").remove("name");
        })
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    let ana = fixture.cookie("ana").await;
    let (status, json) = fixture.put_value(&ana, SHOP, rename).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert!(fixture.text().contains("name = \"store\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn create_needs_create_on_every_group() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    let pia = fixture.cookie("pia").await;
    let vera = fixture.cookie("vera").await;

    for (cookie, groups) in [
        (&eva, ["web", "press"].as_slice()),
        (&eva, &["press"]),
        (&eva, &["internal"]),
        (&pia, &["web"]),
        (&pia, &["press", "web"]),
        (&vera, &["web"]),
    ] {
        let (status, json) = fixture.create(cookie, new_host(Some(groups))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{groups:?}: {json}");
        assert_eq!(fixture.text(), before, "{groups:?}");
    }

    let (status, json) = fixture.create(&eva, new_host(Some(&["web"]))).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    assert_eq!(json, json!({ "key": "0:4" }));
    let files = fixture.get(&eva, "/config/files").await;
    assert_eq!(keys(&files, "server"), [SHOP, BLOG, UNNAMED, "0:4"]);

    // Through the text of the table, and by somebody who will not see it.
    let toml = format!(
        "listen = \"{}\"\ngroups = [\"press\"]\nforward = \"127.0.0.1:9\"\n",
        free_address()
    );
    let body = json!({ "toml": toml });
    let (status, json) = fixture
        .user(&pia, Method::POST, "/config/entities/server", &body)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    let files = fixture.get(&pia, "/config/files").await;
    assert_eq!(keys(&files, "server"), [] as [&str; 0]);
    assert!(fixture.text().contains("groups = [\"press\"]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn create_without_groups_is_refused() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    for value in [new_host(None), new_host(Some(&[]))] {
        let (status, json) = fixture.create(&eva, value).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    }
    let toml = format!(
        "listen = \"{}\"\nforward = \"127.0.0.1:9\"\n",
        free_address()
    );
    let body = json!({ "toml": toml });
    let (status, json) = fixture
        .user(&eva, Method::POST, "/config/entities/server", &body)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    // The token creates a host without groups, as before.
    let body = json!({ "value": new_host(None) });
    let (status, json) = fixture
        .token(Method::POST, "/config/entities/server", &body)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn creating_a_reserved_name_is_refused() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;

    // `shop` is the target of a grant of the role `shop-only`.
    let mut value = new_host(Some(&["web"]));
    value["name"] = json!("shop");
    let (status, json) = fixture.create(&eva, value).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(json, json!({ "error": "this name is reserved" }));
    // A name read from the environment could be any name.
    let mut value = new_host(Some(&["web"]));
    value["name"] = json!("{env.RXH_TEST_HOST_NAME}");
    let (status, json) = fixture.create(&eva, value).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    let mut value = new_host(Some(&["web"]));
    value["name"] = json!("store");
    let (status, json) = fixture.create(&eva, value).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    // The administrator may take the name.
    let ana = fixture.cookie("ana").await;
    let mut value = new_host(None);
    value["name"] = json!("shop-2");
    let (status, json) = fixture.create(&ana, value).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_needs_delete() {
    let fixture = daemon().await;
    let before = fixture.text();
    for name in ["sam", "vera"] {
        let cookie = fixture.cookie(name).await;
        let (status, json) = fixture.delete(&cookie, SHOP).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{name}: {json}");
        assert_eq!(fixture.text(), before, "{name}");
    }

    let eva = fixture.cookie("eva").await;
    let (status, json) = fixture.delete(&eva, UNNAMED).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let text = fixture.text();
    assert!(!text.contains(&fixture.listens[3].to_string()), "{text}");
    assert!(text.contains("name = \"intra\""), "{text}");
    let files = fixture.get(&eva, "/config/files").await;
    assert_eq!(keys(&files, "server"), [SHOP, BLOG]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_grant_does_not_reach_other_hosts_of_the_group() {
    let fixture = daemon().await;
    let before = fixture.text();
    let sam = fixture.cookie("sam").await;
    let files = fixture.get(&sam, "/config/files").await;
    assert_eq!(keys(&files, "server"), [SHOP]);
    assert_eq!(
        server(&files, SHOP).expect("listed")["permissions"],
        json!(["view", "modify"])
    );

    let change = |value: &mut Value| value["max_connections"] = json!(5);
    for key in [BLOG, UNNAMED, INTRA] {
        let (status, json) = fixture.put_value(&sam, key, change).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{key}: {json}");
        let (status, json) = fixture.delete(&sam, key).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{key}: {json}");
    }
    let (status, json) = fixture.create(&sam, new_host(Some(&["web"]))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    let (status, json) = fixture.put_value(&sam, SHOP, change).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    let status = fixture.get(&sam, "/status").await;
    let names: Vec<&Value> = status["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .map(|server| &server["name"])
        .collect();
    assert_eq!(names, [&json!("shop")]);
}

#[tokio::test(flavor = "multi_thread")]
async fn pools_sections_and_whole_files_are_refused() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    let revision = fixture.revision().await;
    let pool =
        json!({ "value": { "backends": ["127.0.0.1:1"] }, "key": "mine", "revision": revision });
    let section = json!({ "value": { "token": "another-token-0123456789" }, "key": "log", "revision": revision });
    let file = json!({ "text": "", "revision": revision });
    let revision = format!("?revision={revision}");

    for (method, uri, body) in [
        (Method::POST, "/config/entities/upstream".to_owned(), &pool),
        (
            Method::PUT,
            "/config/entities/upstream/api".to_owned(),
            &pool,
        ),
        (
            Method::DELETE,
            format!("/config/entities/upstream/api{revision}"),
            &Value::Null,
        ),
        (
            Method::POST,
            "/config/entities/section".to_owned(),
            &section,
        ),
        (
            Method::PUT,
            "/config/entities/section/admin".to_owned(),
            &section,
        ),
        (
            Method::DELETE,
            format!("/config/entities/section/admin{revision}"),
            &Value::Null,
        ),
        // A pool or a section that is not there is refused the same way.
        (
            Method::PUT,
            "/config/entities/upstream/none".to_owned(),
            &pool,
        ),
        (Method::PUT, "/config/files/0".to_owned(), &file),
        (Method::GET, "/config".to_owned(), &Value::Null),
        (Method::GET, "/pools".to_owned(), &Value::Null),
        (Method::GET, "/metrics".to_owned(), &Value::Null),
        (Method::GET, "/certificates".to_owned(), &Value::Null),
    ] {
        let (status, json) = fixture.user(&eva, method.clone(), &uri, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}: {json}");
        assert_eq!(json["error"], NEEDS_ADMIN, "{method} {uri}");
        assert_eq!(fixture.text(), before, "{method} {uri}");
    }

    // The schema is for whoever is signed in, and nothing is for nobody.
    let (status, schema) = fixture
        .user(&eva, Method::GET, "/schema", &Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(schema["properties"].is_object());
    for uri in ["/schema", "/config/files", "/status", "/stats/history"] {
        let (status, _, json) = call(fixture.admin, Method::GET, uri, "", &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}: {json}");
    }
    let (status, _, json) = call(
        fixture.admin,
        Method::POST,
        "/config/entities/server",
        &json!({ "value": new_host(Some(&["web"])) }).to_string(),
        &[],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{json}");
    assert_eq!(fixture.text(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_revision_says_nothing_about_what_changed() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    let stale = json!({ "error": STALE });

    // The same answer for a host of the user, a hidden one and no host.
    for key in [SHOP, INTRA, "0:9"] {
        let body = json!({ "value": fixture.value(SHOP).await, "revision": "0123" });
        let uri = format!("/config/entities/server/{key}");
        let answer = fixture.user(&eva, Method::PUT, &uri, &body).await;
        assert_eq!(answer, (StatusCode::CONFLICT, stale.clone()), "{key}");
        let uri = format!("{uri}?revision=0123");
        let answer = fixture.user(&eva, Method::DELETE, &uri, &Value::Null).await;
        assert_eq!(answer, (StatusCode::CONFLICT, stale.clone()), "{key}");
    }
    assert_eq!(fixture.text(), before);

    // An administrator is told as before.
    let body = json!({ "value": fixture.value(SHOP).await, "revision": "0123" });
    let uri = format!("/config/entities/server/{SHOP}");
    let (status, json) = fixture.token(Method::PUT, &uri, &body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json, json!({ "error": "the file changed, read it again" }));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_history_show_only_viewable_servers_and_no_pools() {
    let fixture = daemon().await;
    let vera = fixture.cookie("vera").await;
    let [shop, blog, intra, unnamed] = fixture.listens;
    for listen in fixture.listens {
        let (status, ..) = util::get(listen, "/").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    let status = fixture.get(&vera, "/status").await;
    let rows: Vec<(String, Value)> = status["servers"]
        .as_array()
        .expect("servers")
        .iter()
        .map(|row| {
            (
                row["listen"].as_str().expect("listen").to_owned(),
                row["name"].clone(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            (shop.to_string(), json!("shop")),
            (blog.to_string(), json!("blog")),
            (unnamed.to_string(), Value::Null),
        ]
    );
    assert_eq!(status["servers"][0]["state"], "listening");
    assert!(status["version"].is_string());
    let text = status.to_string();
    for hidden in ["intra", &intra.to_string(), "rxh.toml", "api", "pid"] {
        assert!(!text.contains(hidden), "{hidden} in {text}");
    }

    // The next sample is taken at most one interval later.
    let series = |history: &Value| -> Vec<String> {
        let samples = history["samples"].as_array().expect("samples");
        let last = samples.last().expect("a sample");
        last["servers"]
            .as_object()
            .expect("servers")
            .keys()
            .cloned()
            .collect()
    };
    let unnamed = unnamed.to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
    loop {
        let (_, history) = fixture
            .token(Method::GET, "/stats/history", &Value::Null)
            .await;
        let series = series(&history);
        if ["shop", "blog", "intra", &unnamed]
            .iter()
            .all(|name| series.iter().any(|key| key == name))
        {
            assert!(history["samples"][0]["pools"].is_object());
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a sample with the four servers within 7s: {history}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let history = fixture.get(&vera, "/stats/history").await;
    assert_eq!(history["interval_seconds"], 5);
    let mut visible = vec!["blog".to_owned(), "shop".to_owned(), unnamed.clone()];
    visible.sort();
    assert_eq!(series(&history), visible);
    for sample in history["samples"].as_array().expect("samples") {
        assert!(sample["time"].is_u64());
        assert_eq!(sample["pools"], json!({}), "{sample}");
        for key in sample["servers"].as_object().expect("servers").keys() {
            assert!(
                ["shop", "blog", unnamed.as_str()].contains(&key.as_str()),
                "{key}"
            );
        }
    }
    let last = history["samples"].as_array().expect("samples").last();
    let bad_gateways = last.expect("a sample")["servers"]["shop"]["status"][4].as_u64();
    assert!(bad_gateways.is_some_and(|count| count > 0), "{history}");

    // Somebody who views no host gets no series and no row.
    let pia = fixture.cookie("pia").await;
    let status = fixture.get(&pia, "/status").await;
    assert_eq!(status["servers"], json!([]));
    let history = fixture.get(&pia, "/stats/history").await;
    assert_eq!(series(&history), [] as [&str; 0]);
    let (status, json) = fixture
        .user(&pia, Method::GET, "/stats/history?since=soon", &Value::Null)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_administrator_user_sees_everything_as_the_token_does() {
    let fixture = daemon().await;
    let ana = fixture.cookie("ana").await;

    let as_token = fixture
        .token(Method::GET, "/config/files", &Value::Null)
        .await;
    let as_ana = fixture
        .user(&ana, Method::GET, "/config/files", &Value::Null)
        .await;
    assert_eq!(as_ana, as_token);
    let files = as_ana.1;
    assert_eq!(keys(&files, "server"), [SHOP, BLOG, INTRA, UNNAMED]);
    assert_eq!(keys(&files, "section"), ["admin"]);
    assert!(
        files["files"][0]["text"]
            .as_str()
            .expect("text")
            .contains("intra")
    );
    assert!(
        files["files"][0]["path"]
            .as_str()
            .expect("path")
            .ends_with("rxh.toml")
    );
    for key in [SHOP, INTRA, UNNAMED] {
        assert_eq!(
            server(&files, key).expect("listed")["permissions"],
            json!(["view", "modify", "create", "delete"]),
            "{key}"
        );
    }

    // Uptime and samples move between two requests: asked until they agree.
    for uri in ["/status", "/stats/history", "/schema", "/config"] {
        let mut answers = Vec::new();
        for _ in 0..5 {
            let first = fixture.token(Method::GET, uri, &Value::Null).await;
            let as_ana = fixture.user(&ana, Method::GET, uri, &Value::Null).await;
            let last = fixture.token(Method::GET, uri, &Value::Null).await;
            if first == last {
                answers.push((as_ana, first));
                break;
            }
        }
        let (as_ana, as_token) = answers.pop().expect("two equal answers for the token");
        assert_eq!(as_ana.0, StatusCode::OK, "{uri}");
        assert_eq!(as_ana, as_token, "{uri}");
    }
    let (_, status) = fixture.token(Method::GET, "/status", &Value::Null).await;
    assert_eq!(status["servers"].as_array().map(Vec::len), Some(4));
    assert_eq!(status["pools"][0]["name"], "api");
    assert!(status["config_path"].is_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn after_each_refused_write_the_file_is_unchanged() {
    let fixture = daemon().await;
    let before = fixture.text();
    let eva = fixture.cookie("eva").await;
    let sam = fixture.cookie("sam").await;
    let pia = fixture.cookie("pia").await;
    let revision = fixture.revision().await;
    let shop = fixture.value(SHOP).await;
    let intra = fixture.value(INTRA).await;
    let with = |value: &Value, key: &str, new: Value| {
        let mut value = value.clone();
        value[key] = new;
        json!({ "value": value, "revision": revision })
    };
    let server = |key: &str| format!("/config/entities/server/{key}");
    let delete = |key: &str| format!("/config/entities/server/{key}?revision={revision}");
    let create = "/config/entities/server".to_owned();

    let refused = [
        (
            &eva,
            Method::PUT,
            server(INTRA),
            with(&intra, "max_connections", json!(1)),
            404,
        ),
        (&eva, Method::DELETE, delete(INTRA), Value::Null, 404),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "name", json!("intra")),
            403,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "groups", json!(["internal"])),
            403,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "groups", json!(["web", "internal"])),
            403,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "groups", json!("web")),
            400,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "name", json!(["shop"])),
            400,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            with(&shop, "listen", json!("nowhere")),
            400,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            json!({ "value": shop }),
            400,
        ),
        (
            &eva,
            Method::PUT,
            server(SHOP),
            json!({ "value": shop, "toml": "", "revision": revision }),
            400,
        ),
        (
            &eva,
            Method::POST,
            create.clone(),
            json!({ "value": new_host(Some(&["internal"])) }),
            403,
        ),
        (
            &eva,
            Method::POST,
            create.clone(),
            json!({ "value": new_host(Some(&["web"])), "file": 7 }),
            404,
        ),
        (
            &eva,
            Method::POST,
            create.clone(),
            json!({ "value": { "groups": ["web"] } }),
            400,
        ),
        (&sam, Method::DELETE, delete(SHOP), Value::Null, 403),
        (
            &sam,
            Method::PUT,
            server(BLOG),
            with(&shop, "name", json!("blog")),
            404,
        ),
        (
            &sam,
            Method::PUT,
            server(SHOP),
            with(&shop, "groups", json!(["web", "press"])),
            403,
        ),
        (
            &pia,
            Method::PUT,
            server(BLOG),
            with(&shop, "name", json!("blog")),
            404,
        ),
        (&pia, Method::DELETE, delete(BLOG), Value::Null, 404),
        (
            &pia,
            Method::POST,
            create.clone(),
            json!({ "value": new_host(Some(&["web"])) }),
            403,
        ),
        (&pia, Method::POST, "/reload".to_owned(), Value::Null, 403),
    ];
    for (cookie, method, uri, body, expected) in refused {
        let (status, json) = fixture.user(cookie, method.clone(), &uri, &body).await;
        assert_eq!(status.as_u16(), expected, "{method} {uri} {body}: {json}");
        assert_eq!(fixture.text(), before, "{method} {uri} {body}");
    }
    // A request without the `Origin` of the page changes nothing either.
    let cookie = [(header::COOKIE, eva.as_str())];
    let body = with(&shop, "max_connections", json!(2)).to_string();
    let (status, _, json) = call(fixture.admin, Method::PUT, &server(SHOP), &body, &cookie).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
    assert_eq!(fixture.text(), before);

    // The running configuration is the one of the file.
    let (_, config) = fixture.token(Method::GET, "/config", &Value::Null).await;
    assert_eq!(config["server"].as_array().map(Vec::len), Some(4));
    assert_eq!(config["server"][0]["name"], "shop");
    assert_eq!(config["server"][0]["groups"], json!(["web"]));
}
