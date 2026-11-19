//! `dist/image-tags.sh`: the tags of the official images published by
//! `.github/workflows/images.yml`.

#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

fn image_tags(args: &[&str]) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/image-tags.sh");
    Command::new("sh").arg(script).args(args).output().unwrap()
}

fn tags(args: &[&str]) -> Vec<String> {
    let output = image_tags(args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn release_moves_the_minor_and_floating_tags() {
    assert_eq!(
        tags(&["0.3.0", "debian", "ghcr.io/o/rxh"]),
        [
            "ghcr.io/o/rxh:0.3.0",
            "ghcr.io/o/rxh:0.3",
            "ghcr.io/o/rxh:latest"
        ]
    );
    assert_eq!(
        tags(&["0.3.1", "alpine", "rxh"]),
        ["rxh:0.3.1-alpine", "rxh:0.3-alpine", "rxh:alpine"]
    );
    assert_eq!(
        tags(&["0.12.0", "distroless", "rxh"]),
        [
            "rxh:0.12.0-distroless",
            "rxh:0.12-distroless",
            "rxh:distroless"
        ]
    );
}

#[test]
fn major_tag_from_one_on() {
    assert_eq!(
        tags(&["1.2.3", "alpine", "rxh"]),
        [
            "rxh:1.2.3-alpine",
            "rxh:1.2-alpine",
            "rxh:1-alpine",
            "rxh:alpine"
        ]
    );
}

#[test]
fn pre_release_gets_only_its_own_tag() {
    assert_eq!(tags(&["0.4.0-rc.1", "debian", "rxh"]), ["rxh:0.4.0-rc.1"]);
    assert_eq!(
        tags(&["1.0.0-beta.2", "distroless", "rxh"]),
        ["rxh:1.0.0-beta.2-distroless"]
    );
}

#[test]
fn every_image_gets_the_tags() {
    assert_eq!(
        tags(&["0.3.0", "debian", "ghcr.io/o/rxh", "docker.io/o/rxh"]),
        [
            "ghcr.io/o/rxh:0.3.0",
            "ghcr.io/o/rxh:0.3",
            "ghcr.io/o/rxh:latest",
            "docker.io/o/rxh:0.3.0",
            "docker.io/o/rxh:0.3",
            "docker.io/o/rxh:latest",
        ]
    );
}

#[test]
fn workspace_version_is_a_release_version() {
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(
        tags(&[version, "debian", "rxh"])[0],
        format!("rxh:{version}")
    );
}

#[test]
fn rejects_bad_input() {
    for (args, message) in [
        (
            &["v0.3.0", "debian", "rxh"][..],
            "not a release version: v0.3.0",
        ),
        (&["0.3", "debian", "rxh"], "not a release version: 0.3"),
        (&["0.3.0.1", "debian", "rxh"], "not a release version"),
        (&["01.3.0", "debian", "rxh"], "not a release version"),
        (&["0.3.0", "scratch", "rxh"], "unknown variant: scratch"),
        (&["0.3.0", "debian"], "usage:"),
    ] {
        let output = image_tags(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
}
