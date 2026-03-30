//! Some nice utilities for writing automated tests for servers and reverse
//! proxies running on the same tokio runtime.

use std::path::Path;

pub mod config;
pub mod http;
pub mod service;
pub mod tcp;

pub fn temp_test_dir() -> tempfile::TempDir {
	let base = Path::new("/tmp/rxh");
	std::fs::create_dir_all(base).expect("failed to create /tmp/rxh for tests");

	tempfile::Builder::new()
		.prefix("rxh-test-")
		.tempdir_in(base)
		.expect("failed to create isolated temp test directory")
}
