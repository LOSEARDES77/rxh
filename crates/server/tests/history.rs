//! One hour of statistics on the admin API.

mod util;

use std::fs;
use std::time::Duration;

use hyper::{Method, StatusCode, header};
use rxh_test_util::free_address;
use serde_json::Value;
use util::admin::{call, get, start};

/// Requests of class 2xx that the last sample counts for `server`.
fn last_2xx(history: &Value, server: &str) -> Option<u64> {
    history["samples"].as_array()?.last()?["servers"][server]["status"][1].as_u64()
}

#[tokio::test(flavor = "multi_thread")]
async fn history_grows_and_counts_requests() {
    let backend = util::backend("hello").await;
    let (admin, web) = (free_address(), free_address());
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!(
            "[admin]\nlisten = \"{admin}\"\n\n\
             [[server]]\nname = \"history-web\"\nlisten = \"{web}\"\nforward = \"{backend}\"\n"
        ),
    )
    .expect("write config");
    let daemon = start(&config_path, admin).await;

    let (status, history) = get(admin, "/stats/history").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(history["interval_seconds"], 5);
    assert_eq!(history["buckets"].as_array().map(Vec::len), Some(11));
    assert!(!history["samples"].as_array().expect("samples").is_empty());

    for _ in 0..3 {
        let (status, ..) = util::get(web, "/").await;
        assert_eq!(status, StatusCode::OK);
    }

    // The next sample is taken at most one interval later.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
    loop {
        let (_, history) = get(admin, "/stats/history").await;
        if last_2xx(&history, "history-web") == Some(3) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "a sample with the 3 requests within 7s: {history}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    daemon.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn token_required() {
    const TOKEN: &str = "history-token-0123456789";
    let admin = free_address();
    let directory = tempfile::tempdir().expect("temp dir");
    let config_path = directory.path().join("rxh.toml");
    fs::write(
        &config_path,
        format!("[admin]\nlisten = \"{admin}\"\ntoken = \"{TOKEN}\"\n"),
    )
    .expect("write config");
    let daemon = start(&config_path, admin).await;

    let (status, ..) = call(admin, Method::GET, "/stats/history", "", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let bearer = format!("Bearer {TOKEN}");
    let authorization = [(header::AUTHORIZATION, bearer.as_str())];
    let (status, _, history) = call(admin, Method::GET, "/stats/history", "", &authorization).await;
    assert_eq!(status, StatusCode::OK);
    assert!(history["samples"].is_array());

    daemon.abort();
}
