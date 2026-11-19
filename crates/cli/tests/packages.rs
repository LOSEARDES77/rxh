//! Release packages built by `.github/workflows/release.yml`: the archive
//! layout of `dist/stage.sh`, the `.deb`/`.rpm` contents of `dist/nfpm.yaml`,
//! the AUR PKGBUILD of `dist/pkgbuild.sh` and the Homebrew formula of
//! `dist/homebrew.sh`.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const RXH: &str = env!("CARGO_BIN_EXE_rxh");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];

fn dist(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../dist")
        .join(file)
}

fn script(name: &str, args: &[&Path]) -> Output {
    Command::new("sh")
        .arg(dist(name))
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn failure(output: &Output, message: &str) {
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

/// Stages the test binary into `dir/name`.
fn stage(dir: &Path, name: &str) -> PathBuf {
    let stage = dir.join(name);
    success(script("stage.sh", &[Path::new(RXH), &stage]));
    stage
}

/// A `sha256sum` file with a distinct checksum for the archive of every target.
fn sums(dir: &Path, version: &str) -> PathBuf {
    let mut sums = String::new();
    for (index, target) in TARGETS.iter().enumerate() {
        let sum = format!("{index}").repeat(64);
        sums.push_str(&format!("{sum}  rxh-{version}-{target}.tar.gz\n"));
    }
    let path = dir.join("SHA256SUMS");
    fs::write(&path, sums).unwrap();
    path
}

fn rxh_validate(config: &Path) -> Output {
    Command::new(RXH)
        .args(["validate", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

#[test]
fn stage_lays_out_the_archive() {
    let dir = TempDir::new().unwrap();
    let stage = stage(dir.path(), "rxh");
    for file in [
        "rxh",
        "rxh.toml",
        "LICENSE",
        "README.md",
        "systemd/rxh.service",
        "systemd/rxh.socket",
        "man/rxh.1",
        "man/rxh-run.1",
        "man/rxh-upstream-add.1",
        "completions/rxh.bash",
        "completions/_rxh",
        "completions/rxh.fish",
    ] {
        let metadata = fs::metadata(stage.join(file)).unwrap_or_else(|err| panic!("{file}: {err}"));
        assert!(metadata.len() > 0, "{file} is empty");
    }
    assert!(
        fs::read_to_string(stage.join("completions/_rxh"))
            .unwrap()
            .starts_with("#compdef rxh")
    );

    // Packages install the binary in /usr/bin, not /usr/local/bin.
    let service = fs::read_to_string(stage.join("systemd/rxh.service")).unwrap();
    assert!(service.contains("\nExecStart=/usr/bin/rxh run --config /etc/rxh/rxh.toml\n"));
    assert!(!service.contains("/usr/local"));
}

#[test]
fn package_configuration_runs_only_the_admin_api() {
    let output = rxh_validate(&dist("rxh.toml"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // The commented example is a working configuration too.
    let dir = TempDir::new().unwrap();
    let example: String = fs::read_to_string(dist("rxh.toml"))
        .unwrap()
        .lines()
        .filter_map(|line| line.strip_prefix("# "))
        .skip_while(|line| *line != "[[server]]")
        .map(|line| format!("{line}\n"))
        .collect();
    assert!(example.contains("[[server]]"), "{example}");
    let path = dir.path().join("rxh.toml");
    fs::write(&path, example).unwrap();
    let output = rxh_validate(&path);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stage_rejects_bad_input() {
    let dir = TempDir::new().unwrap();
    let existing = stage(dir.path(), "rxh");
    failure(
        &script("stage.sh", &[Path::new(RXH), &existing]),
        "already exists",
    );
    failure(
        &script(
            "stage.sh",
            &[&dir.path().join("missing"), &dir.path().join("out")],
        ),
        "not an executable",
    );
    failure(&script("stage.sh", &[Path::new(RXH)]), "usage:");
    assert!(!dir.path().join("out").exists());
}

#[test]
fn nfpm_sources_exist_in_the_stage() {
    let dir = TempDir::new().unwrap();
    let stage = stage(dir.path(), "rxh");
    let config = fs::read_to_string(dist("nfpm.yaml")).unwrap();
    let sources: Vec<&str> = config
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- src: ./"))
        .collect();
    assert!(sources.len() >= 10, "{sources:?}");
    for source in sources {
        match source.strip_suffix("/*.1") {
            Some(man) => assert!(stage.join(man).join("rxh.1").is_file(), "{source}"),
            None => assert!(stage.join(source).is_file(), "{source}"),
        }
    }
}

#[test]
fn pkgbuild_installs_the_archive() {
    let dir = TempDir::new().unwrap();
    let pkgbuild = success(script(
        "pkgbuild.sh",
        &[Path::new(VERSION), &sums(dir.path(), VERSION)],
    ));
    assert!(
        pkgbuild.contains(&format!("\npkgver={VERSION}\n")),
        "{pkgbuild}"
    );
    assert!(pkgbuild.contains(&format!(
        "source_x86_64=(\"https://github.com/LOSEARDES77/rxh/releases/download/v{VERSION}/\
         rxh-{VERSION}-x86_64-unknown-linux-musl.tar.gz\")"
    )));
    assert!(pkgbuild.contains(&format!("sha256sums_x86_64=('{}')", "0".repeat(64))));
    assert!(pkgbuild.contains(&format!("sha256sums_aarch64=('{}')", "1".repeat(64))));

    // Run package() as makepkg would, on the extracted archive.
    let srcdir = dir.path().join("src");
    stage(&srcdir, &format!("rxh-{VERSION}-x86_64-unknown-linux-musl"));
    let pkgdir = dir.path().join("pkg");
    fs::write(dir.path().join("PKGBUILD"), pkgbuild).unwrap();
    let output = Command::new("bash")
        .args(["-euc", "source PKGBUILD && cd src && package"])
        .current_dir(dir.path())
        .env("CARCH", "x86_64")
        .env("pkgdir", &pkgdir)
        .output()
        .unwrap();
    success(output);
    for file in [
        "usr/bin/rxh",
        "etc/rxh/rxh.toml",
        "usr/lib/systemd/system/rxh.service",
        "usr/lib/systemd/system/rxh.socket",
        "usr/share/man/man1/rxh.1",
        "usr/share/man/man1/rxh-run.1",
        "usr/share/bash-completion/completions/rxh",
        "usr/share/zsh/site-functions/_rxh",
        "usr/share/fish/vendor_completions.d/rxh.fish",
        "usr/share/licenses/rxh-bin/LICENSE",
    ] {
        assert!(pkgdir.join(file).is_file(), "{file}");
    }
}

#[test]
fn pkgbuild_version_of_a_pre_release() {
    let dir = TempDir::new().unwrap();
    let pkgbuild = success(script(
        "pkgbuild.sh",
        &[Path::new("1.0.0-rc.1"), &sums(dir.path(), "1.0.0-rc.1")],
    ));
    // pacman sorts 1.0.0rc.1 before 1.0.0; a `-` is not allowed in pkgver.
    assert!(pkgbuild.contains("\npkgver=1.0.0rc.1\n"), "{pkgbuild}");
    assert!(pkgbuild.contains("/download/v1.0.0-rc.1/rxh-1.0.0-rc.1-aarch64-unknown-linux-musl"));
}

#[test]
fn homebrew_formula_has_every_target() {
    let dir = TempDir::new().unwrap();
    let formula = success(script(
        "homebrew.sh",
        &[Path::new(VERSION), &sums(dir.path(), VERSION)],
    ));
    assert!(formula.contains(&format!("  version \"{VERSION}\"\n")));
    for (index, target) in TARGETS.iter().enumerate() {
        let lines = format!(
            "      url \"https://github.com/LOSEARDES77/rxh/releases/download/v{VERSION}/\
             rxh-{VERSION}-{target}.tar.gz\"\n      sha256 \"{}\"\n",
            format!("{index}").repeat(64)
        );
        assert!(formula.contains(&lines), "{target}:\n{formula}");
    }
    // The macOS block holds the darwin archives, the Linux one the musl archives.
    let (macos, linux) = formula.split_once("on_linux do").unwrap();
    assert!(macos.contains("apple-darwin") && !macos.contains("linux-musl"));
    assert!(linux.contains("linux-musl") && !linux.contains("apple-darwin"));
}

#[test]
fn recipes_reject_bad_input() {
    let dir = TempDir::new().unwrap();
    let sums = sums(dir.path(), VERSION);
    let partial = dir.path().join("partial");
    let first_line = fs::read_to_string(&sums)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    fs::write(&partial, format!("{first_line}\n")).unwrap();
    for recipe in ["pkgbuild.sh", "homebrew.sh"] {
        failure(
            &script(recipe, &[Path::new("v0.3.0"), &sums]),
            "not a release version: v0.3.0",
        );
        failure(
            &script(recipe, &[Path::new("0.3"), &sums]),
            "not a release version",
        );
        failure(&script(recipe, &[Path::new(VERSION)]), "usage:");
        failure(
            &script(recipe, &[Path::new(VERSION), &partial]),
            "no checksum for rxh-",
        );
        failure(
            &script(recipe, &[Path::new("9.9.9"), &sums]),
            "no checksum for rxh-9.9.9-",
        );
    }
}
