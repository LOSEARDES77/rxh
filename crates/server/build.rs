use std::{env, fs, path::PathBuf};

use minify_html::{Cfg, minify};

fn main() {
    let explorer_src = PathBuf::from("assets/file_explorer.html");
    let not_found_src = PathBuf::from("assets/404.html");

    println!("cargo:rerun-if-changed={}", explorer_src.display());
    println!("cargo:rerun-if-changed={}", not_found_src.display());
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is not set"));

    minify_file(&explorer_src, &out_dir.join("file_explorer.min.html"));
    minify_file(&not_found_src, &out_dir.join("404.min.html"));
}

fn minify_file(src: &PathBuf, dst: &PathBuf) {
    let content = fs::read(src).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", src.display());
    });

    let mut cfg = Cfg::new();
    cfg.enable_possibly_noncompliant();
    cfg.minify_css = true;
    cfg.minify_js = true;

    let minified = minify(&content, &cfg);

    fs::write(dst, minified).unwrap_or_else(|err| {
        panic!("failed to write {}: {err}", dst.display());
    });
}
