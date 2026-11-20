//! Admin routes the configuration editor is built from: `GET /schema`,
//! `GET /config/files`, and the routes that save a file or one of its
//! tables.

mod util;

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper::{Method, StatusCode, header};
use rxh_server::Daemon;
use rxh_server::config::Config;
use rxh_test_util::free_address;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::net::TcpStream;
use util::admin::{call, get, send, start};

const TOKEN: &str = "0123456789abcdef0123";

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn entity<'a>(json: &'a Value, kind: &str, key: &str) -> &'a Value {
    json["entities"]
        .as_array()
        .expect("entity list")
        .iter()
        .find(|entity| entity["kind"] == kind && entity["key"] == key)
        .unwrap_or_else(|| panic!("no {kind} {key} in {json:#}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_is_the_cli_schema() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(&config_path, format!("[admin]\nlisten = \"{admin}\"\n")).expect("write config");
    let _daemon = start(&config_path, admin).await;

    let documented = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rxh.schema.json");
    let documented: Value =
        serde_json::from_str(&fs::read_to_string(documented).expect("schema file"))
            .expect("schema is JSON");
    let (status, json) = get(admin, "/schema").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json, documented);
}

#[tokio::test(flavor = "multi_thread")]
async fn files_lists_main_and_included_tables() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let main =
        format!("# Main file.\ninclude = [\"conf.d/*.toml\"]\n\n[admin]\nlisten = \"{admin}\"\n");
    fs::write(&config_path, &main).expect("write config");
    fs::create_dir(directory.path().join("conf.d")).expect("conf.d");
    fs::write(
        directory.path().join("conf.d/api.toml"),
        "[upstream.api]\nbackends = [\"127.0.0.1:9\"]\n",
    )
    .expect("write include");
    let _daemon = start(&config_path, admin).await;

    let (status, json) = get(admin, "/config/files").await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(json["editable"], true);
    let files = json["files"].as_array().expect("file list");
    assert_eq!(files.len(), 2, "{json:#}");
    assert_eq!(files[0]["index"], 0);
    assert_eq!(files[0]["text"], main);
    assert!(
        files[0]["path"]
            .as_str()
            .expect("path")
            .ends_with("rxh.toml")
    );
    assert_eq!(files[1]["index"], 1);
    assert!(
        files[1]["path"]
            .as_str()
            .expect("path")
            .ends_with("conf.d/api.toml")
    );
    let pool = entity(&json, "upstream", "api");
    assert_eq!(pool["file"], 1);
    assert_eq!(pool["value"]["backends"][0], "127.0.0.1:9");
    assert_eq!(entity(&json, "section", "admin")["file"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_masked_and_revision_is_of_the_real_text() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    let text = format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n");
    fs::write(&config_path, &text).expect("write config");
    let _daemon = start(&config_path, admin).await;

    let bearer = format!("Bearer {TOKEN}");
    let (status, _, json) = call(
        admin,
        Method::GET,
        "/config/files",
        "",
        &[(header::AUTHORIZATION, &bearer)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert!(!json.to_string().contains(TOKEN), "{json:#}");
    assert!(
        json["files"][0]["text"]
            .as_str()
            .expect("text")
            .contains("token = \"***\"")
    );
    assert_eq!(entity(&json, "section", "admin")["value"]["token"], "***");
    assert_eq!(json["files"][0]["revision"], sha256_hex(text.as_bytes()));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_configuration_file_nothing_is_editable() {
    let admin = free_address();
    let config = Config::from_toml(&format!("[admin]\nlisten = \"{admin}\"\n")).expect("valid");
    let _daemon = tokio::task::spawn(Daemon::from_config(config).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    let (status, json) = get(admin, "/config/files").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json,
        serde_json::json!({ "editable": false, "files": [], "entities": [] })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_token_is_required() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n"),
    )
    .expect("write config");
    let _daemon = start(&config_path, admin).await;

    let bearer = format!("Bearer {TOKEN}");
    for uri in ["/schema", "/config/files"] {
        let (status, json) = get(admin, uri).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}");
        assert!(!json.to_string().contains(TOKEN));
        let (status, _, _) = call(
            admin,
            Method::GET,
            uri,
            "",
            &[(header::AUTHORIZATION, &bearer)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{uri} with the token");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn quoted_pool_names_are_listed() {
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\n\n[upstream.\"a.b\"]\nbackends = [\"127.0.0.1:9\"]\n"
        ),
    )
    .expect("write config");
    let _daemon = start(&config_path, admin).await;

    let (status, json) = get(admin, "/config/files").await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let pool = entity(&json, "upstream", "a.b");
    assert_eq!(pool["file"], 0);
    assert_eq!(pool["value"]["backends"][0], "127.0.0.1:9");
}

/// A daemon on a configuration file in a temporary directory.
struct Proxy {
    directory: TempDir,
    config_path: PathBuf,
    admin: SocketAddr,
    listen: SocketAddr,
}

impl Proxy {
    /// Starts a daemon on the text `main` makes of the admin address and the
    /// address of the server, after writing `included` as `conf.d/api.toml`.
    async fn start(
        main: impl FnOnce(SocketAddr, SocketAddr) -> String,
        included: Option<&str>,
    ) -> Self {
        let admin = free_address();
        let listen = free_address();
        let directory = tempfile::tempdir().expect("temp dir");
        let config_path = directory.path().join("rxh.toml");
        fs::write(&config_path, main(admin, listen)).expect("write config");
        if let Some(included) = included {
            fs::create_dir(directory.path().join("conf.d")).expect("conf.d");
            fs::write(directory.path().join("conf.d/api.toml"), included).expect("write include");
        }
        let _daemon = start(&config_path, admin).await;
        Self {
            directory,
            config_path,
            admin,
            listen,
        }
    }

    /// One pool `api` with `backend` and one server forwarding to it.
    async fn with_pool(backend: SocketAddr) -> Self {
        Self::start(
            |admin, listen| pool_and_server(admin, listen, backend),
            None,
        )
        .await
    }

    fn text(&self) -> String {
        fs::read_to_string(&self.config_path).expect("read config")
    }

    fn included_path(&self) -> PathBuf {
        self.directory.path().join("conf.d/api.toml")
    }

    async fn files(&self) -> Value {
        let (status, json) = get(self.admin, "/config/files").await;
        assert_eq!(status, StatusCode::OK, "{json:#}");
        json
    }

    async fn revision(&self, index: usize) -> String {
        self.files().await["files"][index]["revision"]
            .as_str()
            .expect("revision")
            .to_owned()
    }

    async fn put(&self, uri: &str, body: &Value) -> (StatusCode, Value) {
        send(self.admin, Method::PUT, uri, &body.to_string()).await
    }

    /// Body of `GET /` through the proxy.
    async fn served(&self) -> String {
        let (_, _, body) = util::get(self.listen, "/").await;
        String::from_utf8_lossy(&body).into_owned()
    }

    /// Waits until the proxy answers with `body`: a reload moves the listener
    /// to the new generation while the old one drains.
    async fn serves(&self, body: &str) {
        util::eventually(async || self.served().await == body).await;
    }
}

fn pool_and_server(admin: SocketAddr, listen: SocketAddr, backend: SocketAddr) -> String {
    format!(
        "[admin]\nlisten = \"{admin}\"\n\n# The pool of the API.\n[upstream.api]\n\
         backends = [\"{backend}\"]\n\n[[server]]\nname = \"web\"\nlisten = \"{listen}\"\n\
         forward = \"@api\"\n"
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn saving_a_pool_writes_the_file_and_serves_the_change() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;
    proxy.serves("old").await;

    let body = json!({
        "value": { "backends": [new.to_string()] },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(json, json!({ "saved": true }));

    let text = proxy.text();
    let pool = format!("# The pool of the API.\n[upstream.api]\nbackends = [\"{new}\"]\n");
    assert!(text.contains(&pool), "{text}");
    assert!(!text.contains(&old.to_string()), "{text}");
    proxy.serves("new").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_value_leaves_the_file_untouched() {
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();

    let body = json!({
        "value": { "backends": [old.to_string()], "policy": "nope" },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    let error = json["error"].as_str().expect("error");
    assert!(error.contains("would break the configuration"), "{error}");
    assert!(error.contains("nope"), "{error}");
    assert_eq!(proxy.text(), original);
    assert_eq!(proxy.served().await, "old");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_that_fails_rolls_the_file_back() {
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();
    // Valid as configuration, but the daemon cannot bind it.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let taken = held.local_addr().expect("address");

    let body = json!({ "value": { "listen": taken.to_string(), "forward": "@api" } });
    let (status, json) = send(
        proxy.admin,
        Method::POST,
        "/config/entities/server",
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    assert_eq!(proxy.text(), original);
    assert!(
        fs::read_dir(proxy.directory.path())
            .expect("directory")
            .count()
            == 1,
        "only the configuration file is left"
    );
    proxy.serves("old").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_revision_is_a_conflict() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;
    let revision = proxy.revision(0).await;
    let outside = format!("{}\n# Edited by somebody else.\n", proxy.text());
    fs::write(&proxy.config_path, &outside).expect("outside edit");

    let body = json!({ "value": { "backends": [new.to_string()] }, "revision": revision });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json:#}");
    assert_eq!(json["error"], "the file changed, read it again");
    assert_eq!(proxy.text(), outside);

    let body = json!({ "text": "", "revision": revision });
    let (status, _) = proxy.put("/config/files/0", &body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let uri = format!("/config/entities/upstream/api?revision={revision}");
    let (status, _) = send(proxy.admin, Method::DELETE, &uri, "").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(proxy.text(), outside);
}

#[tokio::test(flavor = "multi_thread")]
async fn toml_body_replaces_the_table() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;

    let body = json!({
        "toml": format!("# Picked by hand.\nbackends = [\"{new}\"]\nretries = 2\n"),
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let text = proxy.text();
    let pool = format!(
        "# The pool of the API.\n[upstream.api]\n# Picked by hand.\nbackends = [\"{new}\"]\n\
         retries = 2\n"
    );
    assert!(text.contains(&pool), "{text}");
    assert!(text.contains("[[server]]\nname = \"web\"\n"), "{text}");
    proxy.serves("new").await;

    // Text that would write another table under the name of this one.
    let body = json!({
        "toml": format!("backends = [\"{old}\"]\n[upstream.other]\nbackends = [\"{old}\"]\n"),
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    assert_eq!(proxy.text(), text);
}

#[tokio::test(flavor = "multi_thread")]
async fn create_then_delete_a_server() {
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();
    let second = free_address();

    let body = json!({
        "value": { "name": "second", "listen": second.to_string(), "forward": "@api" },
    });
    let (status, json) = send(
        proxy.admin,
        Method::POST,
        "/config/entities/server",
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{json:#}");
    assert_eq!(json, json!({ "key": "0:1" }));
    assert!(proxy.text().contains("name = \"second\""));
    util::eventually(async || TcpStream::connect(second).await.is_ok()).await;
    let (_, _, served) = util::get(second, "/").await;
    assert_eq!(&served[..], b"old");

    let (status, json) = send(
        proxy.admin,
        Method::DELETE,
        "/config/entities/server/0:1",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no revision: {json:#}");
    let uri = format!(
        "/config/entities/server/0%3A1?revision={}",
        proxy.revision(0).await
    );
    let (status, json) = send(proxy.admin, Method::DELETE, &uri, "").await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(json, json!({ "saved": true }));
    assert_eq!(proxy.text(), original);
    util::eventually(async || TcpStream::connect(second).await.is_err()).await;
    proxy.serves("old").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn whole_file_put() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();

    let files = proxy.files().await;
    let text = files["files"][0]["text"]
        .as_str()
        .expect("text")
        .replace(&old.to_string(), &new.to_string());
    let body = json!({ "text": text, "revision": files["files"][0]["revision"] });
    let (status, json) = proxy.put("/config/files/0", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(json, json!({ "saved": true }));
    assert_eq!(proxy.text(), text);
    proxy.serves("new").await;

    let revision = proxy.revision(0).await;
    for broken in ["[upstream.api\n", "[upstream.api]\nbackends = 3\n"] {
        let body = json!({ "text": broken, "revision": revision });
        let (status, json) = proxy.put("/config/files/0", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{broken}: {json:#}");
        assert_eq!(proxy.text(), text, "{broken}");
    }
    assert_ne!(text, original);
    assert_eq!(proxy.served().await, "new");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_put_restores_the_secret_of_a_server_only_in_its_place() {
    let old = util::backend("old").await;
    let other = free_address();
    let server = |name: &str, listen: SocketAddr| {
        format!(
            "[[server]]\nname = \"{name}\"\nlisten = \"{listen}\"\nforward = {{ backends = \
             [\"{old}\"], policy = \"cookie\", secret = \"secret-of-server-{name}\" }}\n"
        )
    };
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "[admin]\nlisten = \"{admin}\"\n\n{}\n{}",
                server("a", listen),
                server("b", other)
            )
        },
        None,
    )
    .await;
    let original = proxy.text();

    let files = proxy.files().await;
    let masked = files["files"][0]["text"].as_str().expect("text");
    assert_eq!(masked.matches("secret = \"***\"").count(), 2, "{masked}");
    assert!(!files.to_string().contains("secret-of-server"), "{files:#}");

    // Sent back as it was read: the secrets of the file are kept.
    let revision = files["files"][0]["revision"].clone();
    let body = json!({ "text": masked, "revision": revision });
    let (status, json) = proxy.put("/config/files/0", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(proxy.text(), original);

    // The servers change places: `***` of the first is no longer its secret.
    let (head, servers) = masked.split_once("[[server]]").expect("servers");
    let (first, second) = servers.split_once("[[server]]").expect("two servers");
    let swapped = format!("{head}[[server]]{second}\n[[server]]{first}");
    let body = json!({ "text": swapped, "revision": revision });
    let (status, json) = proxy.put("/config/files/0", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    let error = json["error"].as_str().expect("error");
    assert!(error.contains("type its secret again"), "{error}");
    assert!(!error.contains("secret-of-server"), "{error}");
    assert_eq!(proxy.text(), original);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_included_file_is_edited_in_place() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "# Main file.\ninclude = [\"conf.d/*.toml\"]\n\n[admin]\nlisten = \"{admin}\"\n\n\
                 [[server]]\nlisten = \"{listen}\"\nforward = \"@api\"\n"
            )
        },
        Some(&format!(
            "# The pool of the API.\n[upstream.api]\nbackends = [\"{old}\"]\n"
        )),
    )
    .await;
    let main = proxy.text();
    proxy.serves("old").await;

    // The revision is the one of the file that holds the pool.
    let value = json!({ "backends": [new.to_string()] });
    let body = json!({ "value": value, "revision": proxy.revision(0).await });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{json:#}");

    let body = json!({ "value": value, "revision": proxy.revision(1).await });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert_eq!(proxy.text(), main);
    assert_eq!(
        fs::read_to_string(proxy.included_path()).expect("include"),
        format!("# The pool of the API.\n[upstream.api]\nbackends = [\"{new}\"]\n")
    );
    proxy.serves("new").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn masked_secret_survives_a_save() {
    const DNS_TOKEN: &str = "literal-token-of-the-dns-provider";
    let old = util::backend("old").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "{}\n[upstream.signed]\nbackends = [\"{old}\"]\npolicy = \"cookie\"\n\
                 secret = \"{{env.CARGO_PKG_NAME}}\" # From the environment.\n\n\
                 [acme.dns]\nprovider = \"cloudflare\"\napi_token = \"{DNS_TOKEN}\" # Rotated.\n",
                pool_and_server(admin, listen, old)
            )
        },
        None,
    )
    .await;

    let files = proxy.files().await;
    assert!(!files.to_string().contains(DNS_TOKEN), "{files:#}");
    let mut acme = entity(&files, "section", "acme")["value"].clone();
    assert_eq!(acme["dns"]["api_token"], "***");
    acme["email"] = json!("ops@rxh.test");
    let body = json!({ "value": acme, "revision": files["files"][0]["revision"] });
    let (status, json) = proxy.put("/config/entities/section/acme", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let text = proxy.text();
    assert!(text.contains("email = \"ops@rxh.test\""), "{text}");
    assert!(
        text.contains(&format!("api_token = \"{DNS_TOKEN}\" # Rotated.\n")),
        "{text}"
    );

    // A placeholder is not a secret: it is read, and written back, as it is.
    let files = proxy.files().await;
    let mut signed = entity(&files, "upstream", "signed")["value"].clone();
    assert_eq!(signed["secret"], "{env.CARGO_PKG_NAME}");
    signed["retries"] = json!(2);
    let body = json!({ "value": signed, "revision": files["files"][0]["revision"] });
    let (status, json) = proxy.put("/config/entities/upstream/signed", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let text = proxy.text();
    assert!(text.contains("retries = 2"), "{text}");
    assert!(
        text.contains("secret = \"{env.CARGO_PKG_NAME}\" # From the environment.\n"),
        "{text}"
    );
    assert!(text.contains(DNS_TOKEN), "{text}");

    // `***` where nothing is stored cannot be saved.
    let before = proxy.text();
    let body = json!({
        "key": "fresh",
        "value": { "backends": [old.to_string()], "policy": "cookie", "secret": "***" },
    });
    let (status, json) = send(
        proxy.admin,
        Method::POST,
        "/config/entities/upstream",
        &body.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    assert_eq!(proxy.text(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_validation_error_does_not_show_a_restored_secret() {
    const SECRET: &str = "hmac-secret-of-the-pool";
    let old = util::backend("old").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "[admin]\nlisten = \"{admin}\"\n\n[upstream]\nsigned = {{ backends = [\"{old}\"], \
                 policy = \"cookie\", secret = \"{SECRET}\" }}\n\n\
                 [[server]]\nlisten = \"{listen}\"\nforward = \"@signed\"\n"
            )
        },
        None,
    )
    .await;
    let original = proxy.text();

    // The unknown key is on the line of the secret, which the error of the
    // parser would quote.
    let body = json!({
        "value": {
            "backends": [old.to_string()],
            "policy": "cookie",
            "secret": "***",
            "polcy": "random",
        },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/signed", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    let error = json["error"].as_str().expect("error");
    assert!(error.contains("polcy"), "{error}");
    assert!(!error.contains(SECRET), "{error}");
    assert_eq!(proxy.text(), original);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_secret_in_an_included_file_is_masked() {
    const SECRET: &str = "hmac-secret-of-the-included-pool";
    let old = util::backend("old").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "include = [\"conf.d/*.toml\"]\n\n[admin]\nlisten = \"{admin}\"\n\n\
                 [[server]]\nlisten = \"{listen}\"\nforward = \"@api\"\n"
            )
        },
        Some(&format!(
            "[upstream.api]\nbackends = [\"{old}\"]\npolicy = \"cookie\"\nsecret = \"{SECRET}\"\n"
        )),
    )
    .await;

    let files = proxy.files().await;
    assert!(!files.to_string().contains(SECRET), "{files:#}");
    assert!(
        files["files"][1]["text"]
            .as_str()
            .expect("text")
            .contains("secret = \"***\"")
    );
    let pool = entity(&files, "upstream", "api");
    assert_eq!(pool["value"]["secret"], "***");
    assert!(pool["toml"].as_str().expect("toml").contains("\"***\""));

    // And it is put back when the pool is saved.
    let mut value = pool["value"].clone();
    value["retries"] = json!(1);
    let body = json!({ "value": value, "revision": files["files"][1]["revision"] });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let included = fs::read_to_string(proxy.included_path()).expect("include");
    assert!(
        included.contains(&format!("secret = \"{SECRET}\"")),
        "{included}"
    );
    assert!(included.contains("retries = 1"), "{included}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broken_file_is_reported_without_its_text() {
    const SECRET: &str = "hmac-secret-on-the-broken-line";
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let revision = proxy.revision(0).await;
    // The string of the secret is not closed.
    let broken = format!("{}policy = \"cookie\"\nsecret = \"{SECRET}\n", proxy.text());
    let broken = broken.replace("\n\n[[server]]", "\n[[server]]");
    fs::write(&proxy.config_path, &broken).expect("break the file");

    let (status, json) = get(proxy.admin, "/config/files").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:#}");
    let error = json["error"].as_str().expect("error");
    assert!(error.contains("rxh.toml"), "{error}");
    assert!(error.contains("line "), "{error}");
    assert!(!json.to_string().contains(SECRET), "{json:#}");

    let pool = json!({ "value": { "backends": [old.to_string()] }, "revision": revision });
    let file = json!({ "text": "", "revision": revision });
    for (uri, body) in [
        ("/config/entities/upstream/api", &pool),
        ("/config/files/0", &file),
    ] {
        let (status, json) = proxy.put(uri, body).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{uri}: {json:#}");
        assert!(!json.to_string().contains(SECRET), "{uri}: {json:#}");
        assert_eq!(proxy.text(), broken, "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_included_file_that_does_not_parse_is_named() {
    let old = util::backend("old").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "include = [\"conf.d/*.toml\"]\n\n[admin]\nlisten = \"{admin}\"\n\n\
                 [[server]]\nlisten = \"{listen}\"\nforward = \"@api\"\n"
            )
        },
        Some(&format!("[upstream.api]\nbackends = [\"{old}\"]\n")),
    )
    .await;
    fs::write(
        proxy.included_path(),
        "[upstream.api]\nsecret = \"secret-on-the-broken-line\n",
    )
    .expect("break the include");

    let (status, json) = get(proxy.admin, "/config/files").await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:#}");
    let error = json["error"].as_str().expect("error");
    assert!(error.contains("conf.d/api.toml"), "{error}");
    assert!(!error.contains("secret-on-the-broken-line"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn no_configuration_file_is_409() {
    let admin = free_address();
    let config = Config::from_toml(&format!("[admin]\nlisten = \"{admin}\"\n")).expect("valid");
    let _daemon = tokio::task::spawn(Daemon::from_config(config).run());
    util::eventually(async || TcpStream::connect(admin).await.is_ok()).await;

    let file = json!({ "text": "", "revision": "" }).to_string();
    let pool = json!({ "key": "api", "value": { "backends": [] }, "revision": "" }).to_string();
    for (method, uri, body) in [
        (Method::PUT, "/config/files/0", file.as_str()),
        (Method::POST, "/config/entities/upstream", pool.as_str()),
        (Method::PUT, "/config/entities/upstream/api", pool.as_str()),
        (
            Method::DELETE,
            "/config/entities/upstream/api?revision=0",
            "",
        ),
    ] {
        let (status, json) = send(admin, method.clone(), uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {uri}: {json:#}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn value_and_toml_together_is_400() {
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();
    let revision = proxy.revision(0).await;

    let backends = json!({ "backends": [old.to_string()] });
    let both = json!({ "value": backends, "toml": "backends = []\n", "revision": revision });
    let neither = json!({ "revision": revision });
    let no_revision = json!({ "value": backends });
    for body in [
        both.to_string(),
        neither.to_string(),
        no_revision.to_string(),
        "{".to_owned(),
    ] {
        let (status, json) = send(
            proxy.admin,
            Method::PUT,
            "/config/entities/upstream/api",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json:#}");
    }
    for body in [both.to_string(), neither.to_string(), "{".to_owned()] {
        let (status, json) = send(
            proxy.admin,
            Method::POST,
            "/config/entities/upstream",
            &body,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {json:#}");
    }
    let (status, _) = proxy.put("/config/files/0", &json!({ "text": "" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no revision");
    assert_eq!(proxy.text(), original);
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_file_kind_or_key_is_404() {
    let old = util::backend("old").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();
    let revision = proxy.revision(0).await;

    let pool = json!({ "value": { "backends": [old.to_string()] }, "revision": revision });
    let file = json!({ "text": "", "revision": revision });
    for (uri, body) in [
        ("/config/files/7", &file),
        ("/config/files/first", &file),
        ("/config/entities/pool/api", &pool),
        ("/config/entities/upstream/missing", &pool),
        ("/config/entities/server/0:9", &pool),
        ("/config/entities/server/3:0", &pool),
        ("/config/entities/section/nope", &pool),
    ] {
        let (status, json) = proxy.put(uri, body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}: {json:#}");
    }
    let uri = format!("/config/entities/upstream/missing?revision={revision}");
    let (status, _) = send(proxy.admin, Method::DELETE, &uri, "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        proxy.admin,
        Method::POST,
        "/config/entities/pool",
        &pool.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let in_file_7 = json!({ "key": "b", "value": { "backends": [old.to_string()] }, "file": 7 });
    let (status, _) = send(
        proxy.admin,
        Method::POST,
        "/config/entities/upstream",
        &in_file_7.to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(proxy.text(), original);
}

#[tokio::test(flavor = "multi_thread")]
async fn token_required() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::start(
        |admin, listen| {
            pool_and_server(admin, listen, old)
                .replace("[admin]\n", &format!("[admin]\ntoken = \"{TOKEN}\"\n"))
        },
        None,
    )
    .await;
    let original = proxy.text();
    let bearer = format!("Bearer {TOKEN}");
    let authorized = [(header::AUTHORIZATION, bearer.as_str())];
    let (_, _, files) = call(proxy.admin, Method::GET, "/config/files", "", &authorized).await;
    let revision = files["files"][0]["revision"].as_str().expect("revision");

    let pool = json!({ "value": { "backends": [new.to_string()] }, "revision": revision });
    let file = json!({ "text": "", "revision": revision });
    let delete = format!("/config/entities/upstream/api?revision={revision}");
    for (method, uri, body) in [
        (Method::PUT, "/config/files/0", file.to_string()),
        (Method::POST, "/config/entities/upstream", pool.to_string()),
        (
            Method::PUT,
            "/config/entities/upstream/api",
            pool.to_string(),
        ),
        (Method::DELETE, delete.as_str(), String::new()),
    ] {
        let (status, json) = send(proxy.admin, method.clone(), uri, &body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert!(!json.to_string().contains(TOKEN));
    }
    assert_eq!(proxy.text(), original);

    let (status, _, json) = call(
        proxy.admin,
        Method::PUT,
        "/config/entities/upstream/api",
        &pool.to_string(),
        &authorized,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let text = proxy.text();
    assert!(text.contains(&new.to_string()), "{text}");
    assert!(text.contains(&format!("token = \"{TOKEN}\"")), "{text}");
    proxy.serves("new").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_does_not_reload_twice() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::start(
        |admin, listen| format!("watch = true\n\n{}", pool_and_server(admin, listen, old)),
        None,
    )
    .await;
    let is_live = async |address: SocketAddr| {
        let (_, json) = get(proxy.admin, "/pools/api").await;
        json.to_string().contains(&address.to_string())
    };

    let body = json!({
        "value": { "backends": [new.to_string()] },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    proxy.serves("new").await;

    // A member added at runtime is not in the file: any reload drops it.
    let runtime = free_address();
    let member = json!({ "address": runtime.to_string() }).to_string();
    let (status, _) = send(proxy.admin, Method::POST, "/pools/api/upstreams", &member).await;
    assert_eq!(status, StatusCode::CREATED);
    // Three polls of the watcher.
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert!(
        is_live(runtime).await,
        "the watcher reloaded the saved file again"
    );

    // The same member tells when the watcher does reload.
    fs::write(
        &proxy.config_path,
        format!("{}\n# Edited outside.\n", proxy.text()),
    )
    .expect("outside edit");
    util::eventually(async || !is_live(runtime).await).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_directory_fails_cleanly() {
    use std::os::unix::fs::PermissionsExt;

    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;
    let original = proxy.text();
    let revision = proxy.revision(0).await;
    let directory = proxy.directory.path();
    let set_mode = |mode: u32| {
        fs::set_permissions(directory, fs::Permissions::from_mode(mode)).expect("chmod");
    };
    set_mode(0o555);
    if fs::write(directory.join("probe"), "").is_ok() {
        // Root writes anywhere: nothing to check.
        set_mode(0o755);
        return;
    }

    let body = json!({ "value": { "backends": [new.to_string()] }, "revision": revision });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    set_mode(0o755);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{json:#}");
    // Nothing was written: the answer says which file and why, and does not
    // speak of a broken configuration or of a rollback.
    let error = json["error"].as_str().expect("error");
    let expected = format!(
        "the configuration file {} cannot be written, nothing was changed: ",
        proxy.config_path.display()
    );
    assert!(error.starts_with(&expected), "{error}");
    assert!(error.contains("Permission denied"), "{error}");
    assert!(!error.contains("would break"), "{error}");
    assert!(!error.contains("rollback failed"), "{error}");
    let names: Vec<String> = fs::read_dir(directory)
        .expect("directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(names, ["rxh.toml"]);
    assert_eq!(proxy.text(), original);
    assert_eq!(proxy.served().await, "old");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_quoted_pool_name_is_edited() {
    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "[admin]\nlisten = \"{admin}\"\n\n[upstream.\"a.b\"]\nbackends = [\"{old}\"]\n\n\
                 [[server]]\nlisten = \"{listen}\"\nforward = \"@a.b\"\n"
            )
        },
        None,
    )
    .await;

    let body = json!({
        "value": { "backends": [new.to_string()] },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/a%2Eb", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    let text = proxy.text();
    let pool = format!("[upstream.\"a.b\"]\nbackends = [\"{new}\"]\n");
    assert!(text.contains(&pool), "{text}");
    proxy.serves("new").await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_save_keeps_the_file_private() {
    use std::os::unix::fs::PermissionsExt;

    let old = util::backend("old").await;
    let new = util::backend("new").await;
    let proxy = Proxy::with_pool(old).await;
    fs::set_permissions(&proxy.config_path, fs::Permissions::from_mode(0o600)).expect("chmod");
    let mode = || {
        let metadata = fs::metadata(&proxy.config_path).expect("metadata");
        metadata.permissions().mode() & 0o7777
    };

    let body = json!({
        "value": { "backends": [new.to_string()], "policy": "nope" },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
    assert_eq!(mode(), 0o600, "after a rejected save");

    let body = json!({
        "value": { "backends": [new.to_string()] },
        "revision": proxy.revision(0).await,
    });
    let (status, json) = proxy.put("/config/entities/upstream/api", &body).await;
    assert_eq!(status, StatusCode::OK, "{json:#}");
    assert!(proxy.text().contains(&new.to_string()));
    assert_eq!(mode(), 0o600, "after a save");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_put_refuses_masks_in_servers_that_look_alike() {
    let old = util::backend("old").await;
    // Two servers on one address, told apart by their host only.
    let server = |listen: SocketAddr, host: &str| {
        format!(
            "[[server]]\nlisten = \"{listen}\"\nhost = \"{host}.test\"\nforward = {{ backends = \
             [\"{old}\"], policy = \"cookie\", secret = \"secret-of-server-{host}\" }}\n"
        )
    };
    let proxy = Proxy::start(
        |admin, listen| {
            format!(
                "[admin]\nlisten = \"{admin}\"\n\n{}\n{}",
                server(listen, "a"),
                server(listen, "b")
            )
        },
        None,
    )
    .await;
    let original = proxy.text();

    let files = proxy.files().await;
    let masked = files["files"][0]["text"].as_str().expect("text");
    let (head, servers) = masked.split_once("[[server]]").expect("servers");
    let (first, second) = servers.split_once("[[server]]").expect("two servers");
    let swapped = format!("{head}[[server]]{second}\n[[server]]{first}");
    for text in [swapped.as_str(), masked] {
        let body = json!({ "text": text, "revision": files["files"][0]["revision"] });
        let (status, json) = proxy.put("/config/files/0", &body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{json:#}");
        let error = json["error"].as_str().expect("error");
        assert!(error.contains("type its secret again"), "{error}");
        assert!(!error.contains("secret-of-server"), "{error}");
        assert_eq!(proxy.text(), original);
    }
}
