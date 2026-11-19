//! `rxh fmt` and `rxh schema`.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use rxh_core::config::Config;

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

const MESSY: &str = "# Pools.\n[ upstream.api ]\n  backends=[\"127.0.0.1:3000\"]   # one\n\n\n\
                     [[server]]\n  listen  =  \"127.0.0.1:8080\"\n  forward=\"@api\"\n\n\n";
const FORMATTED: &str = "# Pools.\n[upstream.api]\nbackends = [\"127.0.0.1:3000\"]   # one\n\n\
                         [[server]]\nlisten = \"127.0.0.1:8080\"\nforward = \"@api\"\n";

fn rxh(args: &[&str]) -> Output {
    Command::new(RXH)
        .args(args)
        .env_remove("RXH_CONFIG")
        .output()
        .unwrap()
}

fn config_json(text: &str) -> serde_json::Value {
    serde_json::to_value(Config::from_toml(text).unwrap()).unwrap()
}

#[test]
fn rewrites_files_in_place_without_changing_the_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let messy = dir.path().join("messy.toml");
    fs::write(&messy, MESSY).unwrap();
    let full = dir.path().join("full.toml");
    let full_text: String =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/full.toml"))
            .unwrap()
            .replace("\n", "\n   ");
    fs::write(&full, &full_text).unwrap();

    let output = rxh(&["fmt", messy.to_str().unwrap(), full.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");

    assert_eq!(fs::read_to_string(&messy).unwrap(), FORMATTED);
    let formatted_full: String = fs::read_to_string(&full).unwrap();
    for line in formatted_full.lines() {
        let indented_key: bool = line.starts_with(' ')
            && line
                .trim_start()
                .starts_with(|c: char| c.is_ascii_alphabetic() || c == '[');
        assert!(!indented_key, "{line:?} is still indented");
    }
    assert_eq!(config_json(&formatted_full), config_json(&full_text));

    let output = rxh(&[
        "fmt",
        "--check",
        messy.to_str().unwrap(),
        full.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn check_lists_unformatted_files_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let messy = dir.path().join("messy.toml");
    let clean = dir.path().join("clean.toml");
    fs::write(&messy, MESSY).unwrap();
    fs::write(&clean, FORMATTED).unwrap();

    let output = rxh(&[
        "fmt",
        "--check",
        messy.to_str().unwrap(),
        clean.to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.trim_end(), messy.display().to_string());
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 file(s) not formatted"));
    assert_eq!(fs::read_to_string(&messy).unwrap(), MESSY);
}

#[test]
fn formats_the_configuration_file_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("rxh.toml");
    fs::write(&config, MESSY).unwrap();

    let output = rxh(&["--config", config.to_str().unwrap(), "fmt"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read_to_string(&config).unwrap(), FORMATTED);
}

#[test]
fn leaves_invalid_files_alone() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.toml");
    fs::write(&broken, "  [server\nlisten = 1\n").unwrap();

    let output = rxh(&["fmt", broken.to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("broken.toml"));
    assert_eq!(
        fs::read_to_string(&broken).unwrap(),
        "  [server\nlisten = 1\n"
    );

    let output = rxh(&["fmt", dir.path().join("absent.toml").to_str().unwrap()]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot read"));
}

#[test]
fn schema_prints_the_checked_in_schema() {
    let output = rxh(&["schema"]);
    assert!(output.status.success(), "{output:?}");
    let checked_in: String = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rxh.schema.json"),
    )
    .unwrap();
    assert_eq!(String::from_utf8(output.stdout).unwrap(), checked_in);
}
