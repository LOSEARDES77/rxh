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

    // `since` leaves out what the page already has.
    let (_, all) = get(admin, "/stats/history").await;
    let times = |history: &Value| -> Vec<u64> {
        let samples = history["samples"].as_array().expect("samples");
        samples
            .iter()
            .map(|sample| sample["time"].as_u64().expect("time"))
            .collect()
    };
    let all_times = times(&all);
    assert!(all_times.len() >= 2, "{all_times:?}");
    let (first, newest) = (all_times[0], all_times[all_times.len() - 1]);
    let (status, later) = get(admin, &format!("/stats/history?since={first}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(later["interval_seconds"], 5);
    assert_eq!(later["buckets"], all["buckets"]);
    let later_times = times(&later);
    assert!(later_times.len() < all_times.len(), "{later_times:?}");
    assert!(later_times.contains(&newest), "{later_times:?}");
    assert!(later_times.iter().all(|time| *time > first));
    let (status, none) = get(admin, &format!("/stats/history?since={}", newest + 60)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(none["samples"], serde_json::json!([]));
    assert_eq!(none["buckets"], all["buckets"]);

    for since in ["soon", "", "-1", "1.5"] {
        let uri = format!("/stats/history?since={since}");
        let (status, json) = get(admin, &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
        assert_eq!(
            json["error"], "since must be a unix time in seconds",
            "{uri}"
        );
    }

    // The page asks every 5 seconds: no indentation.
    let (status, _, body) = util::get(admin, "/stats/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains(&b'\n'), "one line");
    assert!(!body.windows(2).any(|pair| pair == b": "), "no padding");

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
