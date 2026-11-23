//! Builds the web UI in `ui/` into `ui/dist/` when its sources change.
//!
//! `src/admin.rs` embeds `ui/dist/` with `include_str!`, so the binary always
//! carries the last build. `ui/dist/` is committed: without `bun` on the
//! `PATH` the committed build is used as is, and building the crate needs no
//! JavaScript tooling.

use std::path::Path;
use std::process::Command;

/// The inputs of `bun run build`, relative to `ui/`. `dist/` is the output and
/// `node_modules/` changes on its own, so neither is watched.
const SOURCES: &[&str] = &[
    "src",
    "index.html",
    "package.json",
    "bun.lock",
    "vite.config.ts",
    "svelte.config.js",
    "tsconfig.json",
];

fn main() {
    let ui = Path::new("ui");
    for source in SOURCES {
        println!("cargo::rerun-if-changed=ui/{source}");
    }
    println!("cargo::rerun-if-env-changed=RXH_SKIP_UI_BUILD");

    if std::env::var_os("RXH_SKIP_UI_BUILD").is_some() {
        return;
    }
    if Command::new("bun").arg("--version").output().is_err() {
        println!("cargo::warning=bun not found: embedding the committed ui/dist");
        return;
    }
    if !ui.join("node_modules").is_dir()
        && let Err(error) = bun(ui, &["install", "--frozen-lockfile"])
    {
        // Offline, for one: the committed build still works.
        println!("cargo::warning=bun install failed, embedding the committed ui/dist: {error}");
        return;
    }
    if let Err(error) = bun(ui, &["run", "build"]) {
        panic!("the web UI in ui/ does not build:\n{error}");
    }
}

/// Runs `bun` with `args` in `dir`, and returns its output when it fails.
///
/// The output is captured: cargo reads the standard output of a build script
/// as instructions.
fn bun(dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("bun")
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "bun {} exited with {}\n{}{}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ))
}
