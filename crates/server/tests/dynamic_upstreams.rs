//! Pool members resolved through DNS.

#![allow(clippy::unwrap_used)]

mod util;

use hyper::StatusCode;

#[tokio::test(flavor = "multi_thread")]
async fn a_records_feed_the_pool() {
    let backend = util::backend("dynamic").await;
    let (proxies, registry) = util::spawn_with_registry(&format!(
        r#"
        [upstream.dns]
        backends = []
        dynamic = {{ a = "localhost:{}", versions = ["ipv4"], refresh = "100ms" }}

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@dns"
        "#,
        backend.port()
    ));
    let _tasks = registry.start_background_tasks();

    let pool = registry.get("dns").unwrap();
    util::eventually(async || !pool.pool.upstreams().is_empty()).await;
    assert_eq!(pool.pool.upstreams()[0].address(), backend.to_string());

    let (status, _, body) = util::get(proxies[0], "/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"dynamic");

    // Refreshes keep the same member, and with it its counters.
    let before = pool.pool.upstreams()[0].clone();
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;
    assert!(std::sync::Arc::ptr_eq(&before, &pool.pool.upstreams()[0]));
    assert_eq!(pool.pool.upstreams().len(), 1);
}
