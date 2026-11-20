//! Connections over time: idle limits, drains on stop and reload, GOAWAY.

use std::net::UdpSocket;
use std::time::Duration;

use hyper::StatusCode;
use rxh_test_util::{Http3Client, RawHttp3Connection, free_address};

use crate::{
    Proxy, arrival, forwarding, get_request, headers_frame, https_get, slow_backend, util,
};

#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_closes_the_connection() {
    let backend = util::backend("idle").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "idle_timeout = \"300ms\"")).await;
    let client = proxy.connect("localhost").await;

    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("an idle connection is closed within 2 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_without_a_request_is_closed() {
    let backend = util::backend("late").await;
    let proxy =
        Proxy::start(|site| forwarding(site, backend, "read_header_timeout = \"300ms\"")).await;
    let client = proxy.connect("localhost").await;

    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("a connection without a request is closed within 2 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_lets_a_request_in_flight_finish() {
    let (backend, mut arrivals) = slow_backend("finished", Duration::from_millis(500)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);

    let (client, response) = request.await.unwrap();
    let response = response.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "finished");
    tokio::time::timeout(Duration::from_secs(2), client.closed())
        .await
        .expect("the stopped daemon closes the connection within 2 s");
    let stopped = tokio::time::timeout(Duration::from_secs(2), proxy.daemon).await;
    stopped.expect("the daemon stops").unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn grace_period_cuts_long_requests() {
    let (backend, mut arrivals) = slow_backend("never", Duration::from_secs(3600)).await;
    let proxy = Proxy::start(|site| {
        format!(
            "grace_period = \"200ms\"\n{}",
            forwarding(site, backend, "")
        )
    })
    .await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move { client.send(get_request()).await });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);

    let stopped = tokio::time::timeout(Duration::from_secs(3), proxy.daemon).await;
    stopped
        .expect("the daemon stops within 3 s")
        .unwrap()
        .unwrap();
    let cut = tokio::time::timeout(Duration::from_secs(3), request).await;
    let cut = cut.expect("the request ends within 3 s").unwrap();
    assert!(cut.is_err(), "a cut request ended normally: {cut:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_keeps_the_connection_and_the_request() {
    let (a, mut arrivals) = slow_backend("a", Duration::from_millis(500)).await;
    let b = util::backend("b").await;
    let proxy = Proxy::start(|site| forwarding(site, a, "")).await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    proxy.reload(|site| forwarding(site, b, "")).await;

    let (mut client, response) = request.await.unwrap();
    let response = response.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "a");
    // The previous generation drains its connections: GOAWAY, then close.
    let refused = client.send(get_request()).await;
    assert!(refused.is_err(), "a draining connection took a request");
    assert_eq!(proxy.get("/").await.body, "b");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_turning_http3_off_closes_the_socket() {
    let backend = util::backend("off").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    assert!(
        UdpSocket::bind(proxy.address).is_err(),
        "HTTP/3 holds the UDP port"
    );

    proxy
        .reload(|site| forwarding(site, backend, "http3 = false"))
        .await;

    let freed = util::eventually(async || UdpSocket::bind(proxy.address).is_ok());
    tokio::time::timeout(Duration::from_secs(2), freed)
        .await
        .expect("the UDP port is free within 2 s");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_to_a_udp_server_on_the_port_works() {
    let (backend, mut arrivals) = slow_backend("never", Duration::from_secs(3600)).await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    // Without a grace period, a reload that has to stop the running
    // configuration before it starts the new one waits for this request.
    let tcp = https_get(&proxy).await;
    arrival(&mut arrivals).await;

    let reload = proxy.reload(|site| {
        format!(
            "[[server]]\nlisten = \"{}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n",
            site.listen
        )
    });
    tokio::time::timeout(Duration::from_secs(2), reload)
        .await
        .expect("the reload completes within 2 s");

    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(b"ping", proxy.address).await.unwrap();
    let mut buffer = [0; 64];
    let (read, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buffer))
        .await
        .expect("an answer within 2 s")
        .unwrap();
    assert_eq!(&buffer[..read], b"e:ping");
    tcp.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn client_without_stream_credit_is_closed() {
    let backend = util::backend("unused").await;
    let timed =
        Proxy::start(|site| forwarding(site, backend, "read_header_timeout = \"300ms\"")).await;
    let silent = timed.connect_silent().await;
    tokio::time::timeout(Duration::from_secs(2), silent.closed())
        .await
        .expect("a connection that grants no stream is closed within 2 s");

    // With the default deadline, stopping does not wait for it either.
    let stopped = Proxy::start(|site| {
        format!(
            "grace_period = \"200ms\"\n{}",
            forwarding(site, backend, "")
        )
    })
    .await;
    let _silent = stopped.connect_silent().await;
    assert_eq!(stopped.admin_post("/stop").await, StatusCode::OK);
    let done = tokio::time::timeout(Duration::from_secs(3), stopped.daemon).await;
    done.expect("the daemon stops within 3 s").unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_reload_keeps_http3() {
    let backend = util::backend("kept").await;
    let echo = util::udp_echo("e:").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;

    // The UDP port goes to a `udp` server, but the configuration cannot
    // start: the certificate file of the other server does not exist.
    let status = proxy
        .try_reload(|site| {
            let missing = site.dir.join("missing.pem");
            format!(
                "[[server]]\nlisten = \"{}\"\nprotocol = \"udp\"\nforward = \"{echo}\"\n\n\
                 [[server]]\nlisten = \"{}\"\nforward = \"{backend}\"\n\
                 tls = {{ cert = {missing:?}, key = {missing:?} }}\n",
                site.listen,
                free_address()
            )
        })
        .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let mut client = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect())
        .await
        .expect("a handshake within 2 s")
        .expect("HTTP/3 still served");
    let response = client.send(get_request()).await.unwrap();
    assert_eq!(response.body, "kept");
}

#[tokio::test(flavor = "multi_thread")]
async fn unvalidated_clients_get_a_retry_when_slots_are_scarce() {
    let backend = util::backend("retried").await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "max_connections = 3")).await;
    let _first = proxy.connect("localhost").await;
    let _second = proxy.connect("localhost").await;

    // One slot of three left: the server asks for a Retry first, which
    // the client answers on its own. That a Retry happened is not visible
    // from the client: the unit test of `needs_retry` covers the decision.
    let mut third = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect())
        .await
        .expect("a handshake within 2 s")
        .expect("the last slot is served");

    let response = third.send(get_request()).await.unwrap();
    assert_eq!(response.body, "retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn reload_off_then_on_reuses_the_socket() {
    let (slow, mut arrivals) = slow_backend("slow", Duration::from_millis(1500)).await;
    let fast = util::backend("fast").await;
    let proxy = Proxy::start(|site| forwarding(site, slow, "")).await;
    let mut client = proxy.connect("localhost").await;
    let request = tokio::task::spawn(async move { client.send(get_request()).await });
    arrival(&mut arrivals).await;

    proxy
        .reload(|site| forwarding(site, fast, "http3 = false"))
        .await;
    proxy.reload(|site| forwarding(site, fast, "")).await;

    // Two sockets on the port would split new clients between them, and
    // the old one takes no new connection: several clients find it.
    for _ in 0..10 {
        let client = tokio::time::timeout(Duration::from_secs(2), proxy.try_connect()).await;
        let mut client = client
            .expect("a handshake within 2 s")
            .expect("a new client is served");
        assert_eq!(client.send(get_request()).await.unwrap().body, "fast");
    }
    let response = request.await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "slow");
}

#[tokio::test(flavor = "multi_thread")]
async fn drain_does_not_wait_forever_for_a_pinging_client() {
    let (backend, mut arrivals) = slow_backend("pinged", Duration::from_millis(300)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "")).await;
    // Pings more often than the server's quiet window: the server keeps
    // acknowledging them.
    let mut client = Http3Client::connect_keep_alive(
        proxy.address,
        "localhost",
        &proxy.root,
        Duration::from_millis(20),
    )
    .await
    .expect("QUIC handshake");

    // The stop comes while the request is in flight, so the connection
    // waits for its response to be acknowledged before it closes.
    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);
    let (client, response) = request.await.unwrap();
    assert_eq!(response.unwrap().status, StatusCode::OK);

    let stopped = tokio::time::timeout(Duration::from_secs(6), proxy.daemon).await;
    stopped
        .expect("the daemon stops within 6 s")
        .unwrap()
        .unwrap();
    drop(client);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_response_survives_the_idle_timeout() {
    let (backend, _arrivals) = slow_backend("slow", Duration::from_millis(1500)).await;
    let proxy = Proxy::start(|site| forwarding(site, backend, "idle_timeout = \"300ms\"")).await;
    // A client that sends no PING while it waits.
    let mut client = proxy.connect("localhost").await;

    let response = tokio::time::timeout(Duration::from_secs(5), client.send(get_request()))
        .await
        .expect("answered within 5 s")
        .expect("the response arrives");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "slow");
}

/// `H3_REQUEST_REJECTED` as quinn prints a stream reset with it.
const REQUEST_REJECTED: &str = "error 267";

#[tokio::test(flavor = "multi_thread")]
async fn goaway_does_not_cover_the_request_in_flight() {
    let (a, mut arrivals) = slow_backend("a", Duration::from_millis(1500)).await;
    let b = util::backend("b").await;
    let proxy = Proxy::start(|site| forwarding(site, a, "")).await;
    let mut client = RawHttp3Connection::connect(proxy.address, "localhost", &proxy.root)
        .await
        .expect("QUIC handshake");
    let (mut send, mut receive) = client.open().await.unwrap();
    send.write_all(&headers_frame("GET", "/", &[]))
        .await
        .unwrap();
    send.finish().unwrap();
    let in_flight = u64::from(send.id());
    arrival(&mut arrivals).await;

    proxy.reload(|site| forwarding(site, b, "")).await;
    let goaway = tokio::time::timeout(Duration::from_secs(2), client.goaway())
        .await
        .expect("a GOAWAY within 2 s")
        .unwrap();

    // Streams at or above the GOAWAY ID were not processed (RFC 9114,
    // section 5.2): a client would send this request again.
    assert!(
        goaway > in_flight,
        "GOAWAY {goaway} covers the request in flight on stream {in_flight}"
    );
    // The next stream, opened after the GOAWAY, is rejected.
    let (mut late, mut late_answer) = client.open().await.unwrap();
    assert!(u64::from(late.id()) >= goaway);
    late.write_all(&headers_frame("GET", "/", &[]))
        .await
        .unwrap();
    late.finish().unwrap();
    let late_answer =
        tokio::time::timeout(Duration::from_secs(2), late_answer.read_to_end(1 << 20))
            .await
            .expect("answered within 2 s");
    let err = late_answer.expect_err("a request after the GOAWAY was served");
    assert!(err.to_string().contains(REQUEST_REJECTED), "{err}");
    // The request in flight finishes.
    let answer = receive.read_to_end(1 << 20).await.unwrap();
    // A DATA frame (type 0) of one byte: the body of the old route.
    let data = [0x00, 0x01, b'a'];
    assert!(answer.windows(3).any(|frame| frame == data), "{answer:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn per_core_stop_lets_a_request_in_flight_finish() {
    let (backend, mut arrivals) = slow_backend("finished", Duration::from_millis(500)).await;
    let proxy =
        Proxy::start(|site| format!("runtime = \"per_core\"\n{}", forwarding(site, backend, "")))
            .await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move { client.send(get_request()).await });
    arrival(&mut arrivals).await;
    assert_eq!(proxy.admin_post("/stop").await, StatusCode::OK);

    let response = request.await.unwrap().unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "finished");
    let stopped = tokio::time::timeout(Duration::from_secs(5), proxy.daemon).await;
    stopped
        .expect("the daemon stops within 5 s")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn per_core_reload_drains_the_old_connection() {
    let (a, mut arrivals) = slow_backend("a", Duration::from_millis(500)).await;
    let b = util::backend("b").await;
    let per_core = |site: &crate::Site, backend| {
        format!("runtime = \"per_core\"\n{}", forwarding(site, backend, ""))
    };
    let proxy = Proxy::start(|site| per_core(site, a)).await;
    let mut client = proxy.connect("localhost").await;

    let request = tokio::task::spawn(async move {
        let response = client.send(get_request()).await;
        (client, response)
    });
    arrival(&mut arrivals).await;
    proxy.reload(|site| per_core(site, b)).await;

    let (client, response) = request.await.unwrap();
    let response = response.unwrap();
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "a");
    tokio::time::timeout(Duration::from_secs(5), client.closed())
        .await
        .expect("the drained connection is closed within 5 s");
    assert_eq!(proxy.get("/").await.body, "b");
}
