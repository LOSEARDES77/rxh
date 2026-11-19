//! OpenTelemetry tracing: W3C trace context propagation (`traceparent`) and
//! one server span per HTTP request, sent in batches to a collector over
//! OTLP/HTTP in JSON.

use std::hash::{BuildHasher, RandomState};
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hyper::HeaderMap;
use hyper::header::{HeaderName, HeaderValue};
use rxh_core::VERSION;
use rxh_core::config::Tracing;
use serde_json::{Value, json};

use crate::hooks::Webhook;

const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");
const TRACESTATE: HeaderName = HeaderName::from_static("tracestate");

/// How long finished spans wait for more before they are sent.
// shortcut: spans finished in the last interval before the process exits are
// lost, flush from the daemon shutdown if that matters.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Spans kept for one batch. More are dropped, so a slow or dead collector
/// cannot grow the memory of the proxy.
const MAX_BATCH: usize = 2048;

/// Trace context of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Trace {
    trace_id: u128,
    /// Span of the caller, from its `traceparent`.
    parent: Option<u64>,
    span_id: u64,
    sampled: bool,
    start: SystemTime,
}

impl Trace {
    /// Trace ID as it appears in `traceparent`.
    pub(crate) fn id(&self) -> String {
        format!("{:032x}", self.trace_id)
    }

    pub(crate) fn is_sampled(&self) -> bool {
        self.sampled
    }

    fn traceparent(&self) -> HeaderValue {
        let value = format!(
            "00-{:032x}-{:016x}-{:02x}",
            self.trace_id,
            self.span_id,
            u8::from(self.sampled)
        );
        HeaderValue::try_from(value).expect("hexadecimal digits and dashes are a valid value")
    }
}

/// What a finished request adds to its span.
pub(crate) struct Span {
    pub(crate) name: String,
    pub(crate) elapsed: Duration,
    /// Marks the span as failed.
    pub(crate) error: bool,
    /// Attributes: JSON strings and integers.
    pub(crate) attributes: Vec<(&'static str, Value)>,
}

/// Traces the requests of one listener according to the `[tracing]` table.
#[derive(Debug)]
pub(crate) struct Tracer {
    collector: Webhook,
    service_name: String,
    sample_ratio: f64,
    /// Finished spans of the next batch, in OTLP JSON.
    pending: Mutex<Vec<Value>>,
    /// True after a failed export, so that a dead collector is reported once.
    failing: AtomicBool,
}

impl Tracer {
    /// Prepares `settings`, which passed configuration validation.
    pub(crate) fn new(settings: &Tracing) -> Self {
        Self {
            collector: Webhook::new(&settings.endpoint),
            service_name: settings.service_name.clone(),
            sample_ratio: settings.sample_ratio,
            pending: Mutex::default(),
            failing: AtomicBool::new(false),
        }
    }

    /// Starts the span of a request: continues the trace of its
    /// `traceparent`, or starts one, and replaces the header with the one
    /// the backends get, which names the new span as their parent.
    pub(crate) fn start(&self, headers: &mut HeaderMap) -> Trace {
        // Several `traceparent` headers are invalid, like a malformed one.
        let mut values = headers.get_all(TRACEPARENT).iter();
        let parent = match (values.next(), values.next()) {
            (Some(value), None) => value.to_str().ok().and_then(parse_traceparent),
            _ => None,
        };
        let span_id = random().max(1);
        let start = SystemTime::now();
        let trace = match parent {
            Some((trace_id, parent, sampled)) => Trace {
                trace_id,
                parent: Some(parent),
                span_id,
                sampled,
                start,
            },
            None => {
                // A `tracestate` belongs to the trace of its `traceparent`.
                headers.remove(TRACESTATE);
                let trace_id = (u128::from(random()) << 64 | u128::from(random())).max(1);
                Trace {
                    trace_id,
                    parent: None,
                    span_id,
                    sampled: self.samples(trace_id),
                    start,
                }
            }
        };
        headers.insert(TRACEPARENT, trace.traceparent());
        trace
    }

    /// Decides from the trace ID, so that every service using the same ratio
    /// keeps the same traces.
    fn samples(&self, trace_id: u128) -> bool {
        // Truncation intended: the low half of the ID.
        let low = trace_id as u64;
        self.sample_ratio >= 1.0 || (low as f64) < self.sample_ratio * 2_f64.powi(64)
    }

    /// Queues the span of a sampled `trace` for the next batch. Must be
    /// called from a Tokio runtime.
    pub(crate) fn finish(self: &Arc<Self>, trace: &Trace, span: Span) {
        if !trace.sampled {
            return;
        }
        let span = otlp_span(trace, span);
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.len() >= MAX_BATCH {
            return;
        }
        pending.push(span);
        if pending.len() == 1 {
            let tracer = Arc::clone(self);
            tokio::task::spawn(async move {
                tokio::time::sleep(FLUSH_INTERVAL).await;
                tracer.flush().await;
            });
        }
    }

    async fn flush(&self) {
        let spans = mem::take(&mut *self.pending.lock().unwrap_or_else(PoisonError::into_inner));
        let count = spans.len();
        let body = json!({
            "resourceSpans": [{
                "resource": {
                    "attributes": [attribute("service.name", self.service_name.as_str().into())],
                },
                "scopeSpans": [{
                    "scope": { "name": "rxh", "version": VERSION },
                    "spans": spans,
                }],
            }],
        });
        let result = self
            .collector
            .post(body.to_string().into_bytes(), "rxh-tracing")
            .await;
        let failed = result.is_err();
        if let Err(err) = result
            && !self.failing.load(Ordering::Relaxed)
        {
            eprintln!(
                "tracing => sending {count} spans to {} failed: {err}; spans are dropped until \
                 it answers",
                self.collector.uri()
            );
        }
        self.failing.store(failed, Ordering::Relaxed);
    }
}

/// A random number. Trace and span IDs and log sampling need no
/// cryptographic strength.
pub(crate) fn random() -> u64 {
    RandomState::new().hash_one(0_u64)
}

/// Parses a `traceparent` into its trace ID, parent span ID and sampled
/// flag. `None` for anything the W3C Trace Context recommendation calls
/// invalid.
fn parse_traceparent(value: &str) -> Option<(u128, u64, bool)> {
    let mut fields = value.trim().split('-');
    let version = hex(fields.next()?, 2)?;
    let trace_id = hex(fields.next()?, 32)?;
    let parent = hex(fields.next()?, 16)?;
    let flags = hex(fields.next()?, 2)?;
    // Later versions may append fields, version 00 has exactly four.
    let valid = version != 0xff
        && trace_id != 0
        && parent != 0
        && (version != 0 || fields.next().is_none());
    let parent = u64::try_from(parent).expect("16 hexadecimal digits fit in 64 bits");
    valid.then_some((trace_id, parent, flags & 1 == 1))
}

/// Parses exactly `len` lowercase hexadecimal digits.
fn hex(field: &str, len: usize) -> Option<u128> {
    let valid = field.len() == len
        && field
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
    valid.then(|| u128::from_str_radix(field, 16).ok())?
}

/// An OTLP JSON attribute. Integers are written as strings, like every
/// 64-bit number in the JSON form of protobuf.
fn attribute(key: &str, value: Value) -> Value {
    let value = match value {
        Value::String(text) => json!({ "stringValue": text }),
        other => json!({ "intValue": other.to_string() }),
    };
    json!({ "key": key, "value": value })
}

fn otlp_span(trace: &Trace, span: Span) -> Value {
    let nanos = |time: SystemTime| {
        let since_epoch = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        since_epoch.as_nanos().to_string()
    };
    let attributes: Vec<Value> = span
        .attributes
        .into_iter()
        .map(|(key, value)| attribute(key, value))
        .collect();
    let mut otlp = json!({
        "traceId": trace.id(),
        "spanId": format!("{:016x}", trace.span_id),
        "name": span.name,
        // SPAN_KIND_SERVER
        "kind": 2,
        "startTimeUnixNano": nanos(trace.start),
        "endTimeUnixNano": nanos(trace.start + span.elapsed),
        "attributes": attributes,
        // STATUS_CODE_ERROR or unset.
        "status": if span.error { json!({ "code": 2 }) } else { json!({}) },
    });
    if let Some(parent) = trace.parent {
        otlp["parentSpanId"] = format!("{parent:016x}").into();
    }
    otlp
}

#[cfg(test)]
mod tests {
    use super::*;

    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    fn tracer(sample_ratio: f64) -> Tracer {
        Tracer::new(&Tracing {
            endpoint: String::from("http://127.0.0.1:4318/v1/traces"),
            service_name: String::from("rxh"),
            sample_ratio,
        })
    }

    #[test]
    fn traceparent_parses_valid_values_only() {
        assert_eq!(
            parse_traceparent(PARENT),
            Some((
                0x4bf9_2f35_77b3_4da6_a3ce_929d_0e0e_4736,
                0x00f0_67aa_0ba9_02b7,
                true
            ))
        );
        let unsampled = PARENT.replace("-01", "-00");
        assert!(matches!(parse_traceparent(&unsampled), Some((_, _, false))));
        // A later version may carry more fields, version 00 may not.
        let later = format!("01{}-extra", &PARENT[2..]);
        assert!(parse_traceparent(&later).is_some());

        for invalid in [
            "",
            "00",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "ff-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92F3577B34DA6A3CE929D0E0E4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-1",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01-extra",
            "00-+bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
        ] {
            assert_eq!(parse_traceparent(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn start_continues_the_trace_of_the_caller() {
        let mut headers = HeaderMap::new();
        headers.insert(TRACEPARENT, HeaderValue::from_static(PARENT));
        headers.insert(TRACESTATE, HeaderValue::from_static("vendor=1"));
        // The ratio only applies to traces started here.
        let trace = tracer(0.0).start(&mut headers);

        assert_eq!(trace.id(), "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(trace.parent, Some(0x00f0_67aa_0ba9_02b7));
        assert!(trace.is_sampled());
        assert_eq!(
            headers[TRACEPARENT],
            format!("00-{}-{:016x}-01", trace.id(), trace.span_id)
        );
        assert_ne!(trace.span_id, 0x00f0_67aa_0ba9_02b7);
        assert_eq!(headers[TRACESTATE], "vendor=1");
    }

    #[test]
    fn start_begins_a_trace_without_a_valid_traceparent() {
        let mut duplicated = HeaderMap::new();
        duplicated.append(TRACEPARENT, HeaderValue::from_static(PARENT));
        duplicated.append(TRACEPARENT, HeaderValue::from_static(PARENT));
        let mut malformed = HeaderMap::new();
        malformed.insert(TRACEPARENT, HeaderValue::from_static("garbage"));
        malformed.insert(TRACESTATE, HeaderValue::from_static("vendor=1"));

        for mut headers in [HeaderMap::new(), duplicated, malformed] {
            let trace = tracer(1.0).start(&mut headers);
            assert_eq!(trace.parent, None);
            assert!(trace.is_sampled());
            assert_ne!(trace.id(), "4bf92f3577b34da6a3ce929d0e0e4736");
            let sent: Vec<&HeaderValue> = headers.get_all(TRACEPARENT).iter().collect();
            assert_eq!(sent, [&trace.traceparent()]);
            assert_eq!(
                parse_traceparent(sent[0].to_str().unwrap()).unwrap().0,
                trace.trace_id
            );
            assert!(!headers.contains_key(TRACESTATE));
        }

        let trace = tracer(0.0).start(&mut HeaderMap::new());
        assert!(!trace.is_sampled());
        assert!(trace.traceparent().to_str().unwrap().ends_with("-00"));
    }

    #[test]
    fn sampling_follows_the_ratio() {
        let half = tracer(0.5);
        assert!(half.samples(1 << 62));
        assert!(!half.samples(3 << 62));
        // Only the low half of the ID counts.
        assert!(half.samples(u128::MAX << 64));
        assert!(tracer(1.0).samples(u128::MAX));
        assert!(!tracer(0.0).samples(1 << 64));
    }

    #[test]
    fn span_is_written_as_otlp_json() {
        let trace = Trace {
            trace_id: 0xabc,
            parent: Some(0xdef),
            span_id: 0x123,
            sampled: true,
            start: UNIX_EPOCH + Duration::from_secs(2),
        };
        let span = Span {
            name: String::from("GET /api"),
            elapsed: Duration::from_millis(5),
            error: true,
            attributes: vec![
                ("http.request.method", "GET".into()),
                ("http.response.status_code", 502.into()),
            ],
        };
        assert_eq!(
            otlp_span(&trace, span),
            json!({
                "traceId": "00000000000000000000000000000abc",
                "spanId": "0000000000000123",
                "parentSpanId": "0000000000000def",
                "name": "GET /api",
                "kind": 2,
                "startTimeUnixNano": "2000000000",
                "endTimeUnixNano": "2005000000",
                "attributes": [
                    { "key": "http.request.method", "value": { "stringValue": "GET" } },
                    { "key": "http.response.status_code", "value": { "intValue": "502" } },
                ],
                "status": { "code": 2 },
            })
        );

        let root = Trace {
            parent: None,
            ..trace
        };
        let span = Span {
            name: String::from("GET"),
            elapsed: Duration::ZERO,
            error: false,
            attributes: Vec::new(),
        };
        let otlp = otlp_span(&root, span);
        assert_eq!(otlp.get("parentSpanId"), None);
        assert_eq!(otlp["status"], json!({}));
    }
}
