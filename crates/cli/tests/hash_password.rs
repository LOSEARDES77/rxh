//! `rxh hash-password`.

#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::process::{Command, Output, Stdio};

use rxh_core::config::Config;

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

fn hash_password(input: &str) -> Output {
    let mut child = Command::new(RXH)
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn prints_a_hash_that_basic_auth_accepts() {
    let output = hash_password("correct horse\n");
    assert!(output.status.success());
    let hash = String::from_utf8(output.stdout).unwrap();
    let hash = hash.trim_end();

    let config = Config::from_toml(&format!(
        "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n\
         basic_auth = {{ users = {{ alice = \"{hash}\" }} }}"
    ))
    .unwrap();
    let basic_auth = config.servers[0].basic_auth.as_ref().unwrap();
    assert!(basic_auth.verify("alice", b"correct horse"));
    assert!(!basic_auth.verify("alice", b"correct horse\n"));
}

#[test]
fn fails_without_a_password() {
    for input in ["", "\n"] {
        let output = hash_password(input);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("no password on standard input"), "{stderr}");
    }
}
