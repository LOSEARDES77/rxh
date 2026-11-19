//! OpenTelemetry tracing: `traceparent` propagation to the backends and
//! spans sent to an OTLP/HTTP collector.

mod util;

use std::fs;
use std::time::Duration;

use bytes::Bytes;
use http::request::Parts;
use http_body_util::Empty;
use hyper::{Request, StatusCode};
use serde_json::Value;
use tokio::sync::mpsc;

const TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const CALLER_SPAN: &str = "00f067aa0ba902b7";

fn request(uri: &str, traceparent: Option<&str>) -> Request<Empty<Bytes>> {
    let mut request = Request::get(uri).header("host", "app.test:8080");
    if let Some(traceparent) = traceparent {
        request = request.header("traceparent", traceparent);
    }
    request.body(Empty::new()).expect("valid request")
}

/// Splits the `traceparent` a backend received into trace ID, span ID and
/// flags.
fn traceparent(parts: &Parts) -> (String, String, String) {
    let value = parts.headers["traceparent"].to_str().expect("ascii");
    let fields: Vec<&str> = value.split('-').collect();
    assert_eq!(fields.len(), 4, "{value}");
    assert_eq!(fields[0], "00", "{value}");
    assert_eq!((fields[1].len(), fields[2].len()), (32, 16), "{value}");
    (
        fields[1].to_owned(),
        fields[2].to_owned(),
        fields[3].to_owned(),
    )
}

/// Waits for `count` spans, over as many exports as they take.
async fn spans(collector: &mut mpsc::Receiver<(Parts, Bytes)>, count: usize) -> Vec<Value> {
    let mut spans = Vec::new();
    while spans.len() < count {
        let (parts, body) = tokio::time::timeout(Duration::from_secs(5), collector.recv())
            .await
            .expect("spans are exported within 5s")
            .expect("collector is running");
        assert_eq!(parts.method, "POST");
        assert_eq!(parts.uri.path(), "/v1/traces");
        assert_eq!(parts.headers["content-type"], "application/json");
        let export: Value = serde_json::from_slice(&body).expect("OTLP JSON");
        let resource = &export["resourceSpans"][0];
        assert_eq!(
            resource["resource"]["attributes"][0],
            serde_json::json!({ "key": "service.name", "value": { "stringValue": "edge" } })
        );
        let batch = resource["scopeSpans"][0]["spans"]
            .as_array()
            .expect("spans");
        spans.extend(batch.iter().cloned());
    }
    spans
}

fn attribute<'a>(span: &'a Value, key: &str) -> &'a Value {
    let attributes = span["attributes"].as_array().expect("attributes");
    let attribute = attributes.iter().find(|attribute| attribute["key"] == key);
    &attribute.unwrap_or_else(|| panic!("no {key} in {span}"))["value"]
}

#[tokio::test(flavor = "multi_thread")]
async fn spans_are_exported_and_the_trace_continues_to_the_backend() {
    let (backend, mut received) = util::interceptor().await;
    let (collector, mut exported) = util::body_interceptor(200).await;
    let directory = tempfile::tempdir().expect("temp dir");
    let log_path = directory.path().join("access.log");
    let dead = util::dead_address();
    let proxy = util::spawn(&format!(
        r#"
[tracing]
endpoint = "http://{collector}/v1/traces"
service_name = "edge"

[log]
format = "json"
output = '{log}'

[[server]]
name = "traced"
listen = "127.0.0.1:0"
[[server.match]]
uri = "/down"
forward = "{dead}"
[[server.match]]
uri = "/api"
forward = "{backend}"
"#,
        log = log_path.display()
    ))[0];

    // A caller that is already tracing: same trace, the proxy span in between.
    let caller = format!("00-{TRACE_ID}-{CALLER_SPAN}-01");
    let (status, ..) = util::send(proxy, request("/api/users?page=2", Some(&caller))).await;
    assert_eq!(status, StatusCode::OK);
    let (trace_id, proxy_span, flags) = traceparent(&received.recv().await.expect("forwarded"));
    assert_eq!((trace_id.as_str(), flags.as_str()), (TRACE_ID, "01"));
    assert_ne!(proxy_span, CALLER_SPAN);

    // No trace yet, and a malformed header: the proxy starts one.
    let (status, ..) = util::send(proxy, request("/api", Some("00-zz-01"))).await;
    assert_eq!(status, StatusCode::OK);
    let (new_trace, new_span, flags) = traceparent(&received.recv().await.expect("forwarded"));
    assert_ne!(new_trace, TRACE_ID);
    assert_eq!(flags, "01");

    let (status, ..) = util::send(proxy, request("/down", None)).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, ..) = util::send(proxy, request("/nowhere", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let spans = spans(&mut exported, 4).await;
    assert_eq!(spans.len(), 4);
    let by_path = |path: &str| {
        let found = spans
            .iter()
            .find(|span| attribute(span, "url.path")["stringValue"] == path);
        found.unwrap_or_else(|| panic!("no span for {path}"))
    };

    let span = by_path("/api/users");
    assert_eq!(span["traceId"], TRACE_ID);
    assert_eq!(span["parentSpanId"], CALLER_SPAN);
    assert_eq!(span["spanId"], proxy_span.as_str());
    assert_eq!(span["name"], "GET /api");
    assert_eq!(span["kind"], 2);
    assert_eq!(span["status"], serde_json::json!({}));
    let nanos = |key: &str| -> u128 { span[key].as_str().expect("string").parse().expect("int") };
    assert!(nanos("startTimeUnixNano") > 0);
    assert!(nanos("endTimeUnixNano") >= nanos("startTimeUnixNano"));
    for (key, value) in [
        ("http.request.method", "GET"),
        ("url.scheme", "http"),
        ("url.query", "page=2"),
        ("network.protocol.version", "1.1"),
        ("server.address", "app.test"),
        ("client.address", "127.0.0.1"),
        ("http.route", "/api"),
        ("rxh.server", "traced"),
        ("rxh.upstream", backend.to_string().as_str()),
    ] {
        assert_eq!(attribute(span, key)["stringValue"], value, "{key}");
    }
    assert_eq!(
        attribute(span, "http.response.status_code")["intValue"],
        "200"
    );

    let span = by_path("/api");
    assert_eq!(span["traceId"], new_trace.as_str());
    assert_eq!(span["spanId"], new_span.as_str());
    assert_eq!(span.get("parentSpanId"), None);

    // 5xx answers mark the span as failed, 4xx ones do not.
    let span = by_path("/down");
    assert_eq!(span["status"]["code"], 2);
    assert_eq!(
        attribute(span, "http.response.status_code")["intValue"],
        "502"
    );
    let span = by_path("/nowhere");
    assert_eq!(span["name"], "GET");
    assert_eq!(span["status"], serde_json::json!({}));

    // JSON access lines name the trace.
    util::eventually(async || {
        fs::read_to_string(&log_path).is_ok_and(|log| log.lines().count() >= 4)
    })
    .await;
    let log = fs::read_to_string(&log_path).expect("log");
    let line: Value = log
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .find(|line: &Value| line["uri"] == "/api/users?page=2")
        .expect("request logged");
    assert_eq!(line["trace_id"], TRACE_ID);
}

#[tokio::test(flavor = "multi_thread")]
async fn sample_ratio_zero_only_exports_traces_the_caller_sampled() {
    let (backend, mut received) = util::interceptor().await;
    let (collector, mut exported) = util::body_interceptor(200).await;
    let proxy = util::spawn(&format!(
        r#"
[tracing]
endpoint = "http://{collector}/v1/traces"
service_name = "edge"
sample_ratio = 0.0

[[server]]
listen = "127.0.0.1:0"
forward = "{backend}"
"#
    ))[0];

    // Not sampled: the backends still get a trace context, flagged so.
    util::send(proxy, request("/unsampled", None)).await;
    let (.., flags) = traceparent(&received.recv().await.expect("forwarded"));
    assert_eq!(flags, "00");
    let unsampled = format!("00-{TRACE_ID}-{CALLER_SPAN}-00");
    util::send(proxy, request("/unsampled", Some(&unsampled))).await;
    let (trace_id, _, flags) = traceparent(&received.recv().await.expect("forwarded"));
    assert_eq!((trace_id.as_str(), flags.as_str()), (TRACE_ID, "00"));

    let sampled = format!("00-{TRACE_ID}-{CALLER_SPAN}-01");
    util::send(proxy, request("/sampled", Some(&sampled))).await;

    let spans = spans(&mut exported, 1).await;
    assert_eq!(spans.len(), 1);
    assert_eq!(attribute(&spans[0], "url.path")["stringValue"], "/sampled");
    // Nothing else follows.
    let more = tokio::time::timeout(Duration::from_millis(1500), exported.recv()).await;
    assert!(more.is_err(), "unexpected export: {more:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_served_while_the_collector_is_down() {
    let (backend, mut received) = util::interceptor().await;
    let collector = util::dead_address();
    let proxy = util::spawn(&format!(
        r#"
[tracing]
endpoint = "http://{collector}/v1/traces"

[[server]]
listen = "127.0.0.1:0"
forward = "{backend}"
"#
    ))[0];

    for _ in 0..2 {
        let (status, _, body) = util::send(proxy, request("/", None)).await;
        assert_eq!((status, &body[..]), (StatusCode::OK, &b"intercepted"[..]));
        traceparent(&received.recv().await.expect("forwarded"));
        // Long enough for the failed export of the first request.
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn traceparent_is_left_alone_without_tracing() {
    let (backend, mut received) = util::interceptor().await;
    let proxy = util::spawn(&format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n"
    ))[0];

    let caller = format!("00-{TRACE_ID}-{CALLER_SPAN}-01");
    util::send(proxy, request("/", Some(&caller))).await;
    let parts = received.recv().await.expect("forwarded");
    assert_eq!(parts.headers["traceparent"], caller.as_str());

    util::send(proxy, request("/", None)).await;
    let parts = received.recv().await.expect("forwarded");
    assert!(!parts.headers.contains_key("traceparent"));
}
