//! Admin routes the configuration editor is built from: `GET /schema` and
//! `GET /config/files`.

mod util;

use std::fs;
use std::path::Path;

use hyper::{Method, StatusCode, header};
use rxh_server::Daemon;
use rxh_server::config::Config;
use rxh_test_util::free_address;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use util::admin::{call, get, start};

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
