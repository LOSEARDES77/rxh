//! `rxh completions` and `rxh man`.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::process::{Command, Output};

const RXH: &str = env!("CARGO_BIN_EXE_rxh");

fn rxh(args: &[&str]) -> Output {
    Command::new(RXH)
        .args(args)
        .env_remove("RXH_CONFIG")
        .output()
        .unwrap()
}

fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn prints_a_script_for_every_shell() {
    for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
        let script = stdout(rxh(&["completions", shell]));
        for word in ["upstream", "reverse-proxy", "admin-token"] {
            assert!(script.contains(word), "{shell} script lacks {word}");
        }
    }
}

/// Loads the bash script into bash and asks it to complete a command line.
fn bash_complete(words: &str) -> Vec<String> {
    let script = stdout(rxh(&["completions", "bash"]));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rxh.bash");
    fs::write(&path, script).unwrap();
    let words: Vec<&str> = words.split(' ').collect();
    let program = format!(
        "source {}; COMP_WORDS=({}); COMP_CWORD={}; COMP_LINE='{}'; COMP_POINT=${{#COMP_LINE}}; \
         _rxh rxh '{}' '{}'; printf '%s\\n' \"${{COMPREPLY[@]}}\"",
        path.display(),
        words.join(" "),
        words.len() - 1,
        words.join(" "),
        words[words.len() - 1],
        words[words.len() - 2],
    );
    let output = Command::new("bash")
        .args(["--norc", "-c", &program])
        .output()
        .unwrap();
    stdout(output).lines().map(String::from).collect()
}

#[test]
fn bash_completes_subcommands_and_flags() {
    let commands = bash_complete("rxh up");
    assert_eq!(commands, ["upgrade", "upstream"]);

    let pool_commands = bash_complete("rxh pool r");
    assert_eq!(pool_commands, ["remove", "rm"]);

    let shells = bash_complete("rxh completions ");
    for shell in ["bash", "zsh", "fish"] {
        assert!(shells.iter().any(|word| word == shell), "{shells:?}");
    }

    let flags = bash_complete("rxh status --j");
    assert_eq!(flags, ["--json"]);
}

#[test]
fn rejects_an_unknown_shell() {
    let output = rxh(&["completions", "tcsh"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid value 'tcsh'"), "{stderr}");
}

#[test]
fn prints_the_main_man_page() {
    let page = stdout(rxh(&["man"]));
    assert!(page.contains(".TH rxh 1"), "{page}");
    for word in ["upstream", "reverse\\-proxy", "\\-\\-admin\\-token"] {
        assert!(page.contains(word), "page lacks {word}");
    }
}

#[test]
fn writes_one_page_per_command() {
    let dir = tempfile::tempdir().unwrap();
    let pages = dir.path().join("man1");
    stdout(rxh(&["man", "--dir", pages.to_str().unwrap()]));

    for name in [
        "rxh.1",
        "rxh-upstream.1",
        "rxh-pool-add.1",
        "rxh-reverse-proxy.1",
    ] {
        assert!(pages.join(name).is_file(), "{name} missing");
    }
    let page = fs::read_to_string(pages.join("rxh-upstream-add.1")).unwrap();
    let title = format!(
        ".TH rxh-upstream-add 1  \"rxh {}\"",
        env!("CARGO_PKG_VERSION")
    );
    assert!(page.contains(&title), "{page}");
    assert!(page.contains("Add a backend to a pool"), "{page}");
}

#[test]
fn fails_when_the_directory_cannot_be_created() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file");
    fs::write(&file, "").unwrap();
    let output = rxh(&["man", "--dir", file.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot write the man pages"), "{stderr}");
}
