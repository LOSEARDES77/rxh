//! Maglev hashing and bounded loads for the hash policies, end to end.

#![allow(clippy::unwrap_used)]

mod util;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::{Request, Response, StatusCode};
use tokio::sync::watch;

async fn get_as(proxy: SocketAddr, user: &str, path: &str) -> Bytes {
    let request = Request::get(path)
        .header("x-user", user)
        .body(Empty::<Bytes>::new())
        .unwrap();
    let (code, _, body) = util::send(proxy, request).await;
    assert_eq!(code, StatusCode::OK);
    body
}

#[tokio::test]
async fn maglev_is_sticky_and_spreads_keys() {
    let list = format!(
        "\"{}\", \"{}\", \"{}\"",
        util::backend("a").await,
        util::backend("b").await,
        util::backend("c").await
    );
    let proxy = util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "header"
        key = "x-user"
        hash_type = "maglev"
        backends = [{list}]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#
    ))[0];

    let mut owners = HashSet::new();
    for user in 0..24 {
        let user = format!("user-{user}");
        let first = get_as(proxy, &user, "/").await;
        assert_eq!(first, get_as(proxy, &user, "/").await, "{user} must stick");
        owners.insert(first);
    }
    assert_eq!(owners.len(), 3, "every backend should own some keys");
}

#[tokio::test]
async fn busy_backend_passes_its_keys_on_until_it_is_free() {
    // Backends that keep `/hold` requests open until `release` is set.
    let held = Arc::new(AtomicUsize::new(0));
    let (release, released) = watch::channel(false);
    let mut list = Vec::new();
    for name in ["a", "b"] {
        let held = Arc::clone(&held);
        let released = released.clone();
        let address = util::backend_with(move |request| {
            let held = Arc::clone(&held);
            let mut released = released.clone();
            async move {
                if request.uri().path() == "/hold" {
                    held.fetch_add(1, Ordering::Relaxed);
                    released.wait_for(|released| *released).await.unwrap();
                }
                Response::new(Full::new(Bytes::from(name)))
            }
        })
        .await;
        list.push(format!("\"{address}\""));
    }
    let proxy = util::spawn(&format!(
        r#"
        [upstream.api]
        policy = "header"
        key = "x-user"
        hash_type = "maglev"
        hash_balance_factor = 100
        backends = [{}]

        [[server]]
        listen = "127.0.0.1:0"
        forward = "@api"
        "#,
        list.join(", ")
    ))[0];

    let owner = get_as(proxy, "alice", "/").await;
    assert_eq!(owner, get_as(proxy, "alice", "/").await);

    // One request in flight on the owner: a second one would put it above
    // the average of one request per backend, so it goes to the other.
    let holding = tokio::spawn(get_as(proxy, "alice", "/hold"));
    util::eventually(async || held.load(Ordering::Relaxed) == 1).await;
    let other = get_as(proxy, "alice", "/").await;
    assert_ne!(other, owner);
    assert_eq!(other, get_as(proxy, "alice", "/").await);

    release.send(true).unwrap();
    assert_eq!(holding.await.unwrap(), owner);
    assert_eq!(owner, get_as(proxy, "alice", "/").await);
}
