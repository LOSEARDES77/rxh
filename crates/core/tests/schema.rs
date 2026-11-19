//! JSON schema of the configuration file (`rxh schema`).

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use rxh_core::config::Config;
use serde_json::Value;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn validator() -> Validator {
    let schema: Value = serde_json::to_value(Config::json_schema()).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

/// Validation errors of a TOML document, as `path: message` lines.
fn errors(validator: &Validator, toml_text: &str) -> Vec<String> {
    let instance: Value = toml::from_str(toml_text).unwrap();
    validator
        .iter_errors(&instance)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect()
}

fn toml_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path: PathBuf = entry.unwrap().path();
        if path.is_dir() {
            toml_files(&path, files);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            files.push(path);
        }
    }
}

#[test]
fn checked_in_schema_is_current() {
    let path: PathBuf = workspace_root().join("docs/rxh.schema.json");
    let generated: String = format!(
        "{}\n",
        serde_json::to_string_pretty(&Config::json_schema()).unwrap()
    );
    let checked_in: String = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "{} is out of date, regenerate it with `rxh schema > docs/rxh.schema.json`",
        path.display()
    );
}

#[test]
fn example_configurations_are_valid() {
    let validator: Validator = validator();
    let mut files: Vec<PathBuf> = Vec::new();
    toml_files(&workspace_root().join("examples"), &mut files);
    assert!(files.len() >= 4, "the examples directory moved: {files:?}");
    for file in files {
        let text: String = fs::read_to_string(&file).unwrap();
        assert_eq!(
            errors(&validator, &text),
            Vec::<String>::new(),
            "{}",
            file.display()
        );
    }
}

#[test]
fn accepts_every_form_the_parser_accepts() {
    let validator: Validator = validator();
    let config = r#"
        grace_period = "1m30s"

        [upstream.api]
        backends = ["127.0.0.1:3000"]
        dial_timeout = 5
        keepalive = "1.5s"
        request_buffers = "64KiB"
        response_buffers = 4096

        [[server]]
        listen = "127.0.0.1:8080"
        host = "example.com"
        tls = "auto"
        forward = "@api"

        [[server]]
        listen = ["127.0.0.1:8443", "[::1]:8443"]
        host = ["a.example.com", "b.example.com"]
        tls = { cert = "cert.pem", key = "key.pem" }
        forward = { backends = ["127.0.0.1:3001"], policy = "first" }
        request = { max_body_size = "10MB" }
    "#;
    Config::from_toml(config).unwrap();
    assert_eq!(errors(&validator, config), Vec::<String>::new());
}

#[test]
fn rejects_what_the_parser_rejects() {
    let validator: Validator = validator();
    let server = "[[server]]\nlisten = \"127.0.0.1:8080\"\nforward = \"127.0.0.1:3000\"\n";
    for (invalid, expected) in [
        ("unknown = 1\n", ": "),
        ("grace_period = \"5 parsecs\"\n", "/grace_period"),
        ("grace_period = true\n", "/grace_period"),
        ("runtime = \"threads\"\n", "/runtime"),
        (&format!("{server}tls = \"manual\"\n"), "/server/0/tls"),
        (
            &format!("{server}tls = {{ cert = \"c.pem\" }}\n"),
            "/server/0/tls",
        ),
        (&format!("{server}lisen = \"127.0.0.1:1\"\n"), "/server/0: "),
        (
            "[upstream.api]\nbackends = []\nrequest_buffers = \"10 MB of\"\n",
            "/upstream/api/request_buffers",
        ),
    ] {
        assert!(
            Config::from_toml(invalid).is_err(),
            "the parser accepts {invalid:?}"
        );
        let errors: Vec<String> = errors(&validator, invalid);
        assert!(
            errors.iter().any(|error| error.starts_with(expected)),
            "{invalid:?} should fail at {expected}: {errors:?}"
        );
    }
}
