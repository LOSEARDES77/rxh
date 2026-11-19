//! Replica listeners, the building block of `runtime = "per_core"`: several
//! `SO_REUSEPORT` listeners on one address sharing routes and limits. The
//! threads and runtimes themselves are tested with the daemon, in
//! `crates/cli/tests`.

mod util;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rxh_server::Master;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Runs `text` on a master and one replica of it, as two runtimes would.
fn spawn_replicated(text: &str) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
    let master = Master::from_config(&util::config(text)).expect("valid config");
    let replicas = master.replicas();
    let replica =
        Master::from_replicas(&replicas, Arc::clone(master.registry())).expect("replica binds");
    let sockets = (master.sockets(), replica.sockets());
    tokio::task::spawn(master.run());
    tokio::task::spawn(replica.run());
    sockets
}

/// Sends one request on a new connection and reads the whole response.
async fn request(address: SocketAddr) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connects");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n")
        .await
        .expect("writes");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("reads to the end");
    response
}

#[tokio::test(flavor = "multi_thread")]
async fn replicas_listen_on_the_address_the_first_listener_got() {
    let backend = util::backend("hello").await;
    let (first, second) = spawn_replicated(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n"
    ));
    assert_ne!(first[0].port(), 0);
    assert_eq!(first, second, "port 0 is resolved once");

    // Whichever listener the kernel picks, the request is served.
    for _ in 0..32 {
        assert!(request(first[0]).await.ends_with("hello"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn udp_listeners_are_not_replicated() {
    let backend = util::backend("hello").await;
    let echo = util::udp_echo("udp").await;
    let (first, second) = spawn_replicated(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n\
         [[server]]\nprotocol = \"udp\"\nlisten = \"127.0.0.1:0\"\nforward = \"{echo}\"\n"
    ));
    assert_eq!(first.len(), 2);
    assert_eq!(second, [first[0]], "only the http listener");
}

#[tokio::test(flavor = "multi_thread")]
async fn max_connections_is_shared_by_the_replicas() {
    let backend = util::backend("hello").await;
    let (sockets, _) = spawn_replicated(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nmax_connections = 1\nforward = \"{backend}\"\n"
    ));
    let address = sockets[0];

    // Takes the only slot, on one of the two listeners.
    let mut holder = TcpStream::connect(address).await.expect("connects");
    holder
        .write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n")
        .await
        .expect("writes");
    let mut head = [0; 12];
    holder.read_exact(&mut head).await.expect("served");
    assert_eq!(&head, b"HTTP/1.1 200");

    // Enough connections to reach both listeners: none is served meanwhile.
    let mut waiting = tokio::task::JoinSet::new();
    for _ in 0..16 {
        waiting.spawn(request(address));
    }
    let early = tokio::time::timeout(Duration::from_millis(300), waiting.join_next()).await;
    assert!(early.is_err(), "served over the limit: {early:?}");

    // They are served one after the other once the slot is free.
    drop(holder);
    let mut served = 0;
    while let Some(response) = waiting.join_next().await {
        assert!(response.expect("no panic").ends_with("hello"));
        served += 1;
    }
    assert_eq!(served, 16);
}
