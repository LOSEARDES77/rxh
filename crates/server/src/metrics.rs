//! Prometheus metrics in the text exposition format 0.0.4, written by hand.
//!
//! Request and connection counters live in a process wide registry keyed by
//! label values, so they survive configuration reloads. Upstream metrics are
//! read from the live pools when scraped.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crate::registry::Registry;

/// Upper bounds of the request duration histogram, in seconds.
const BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

#[derive(Default)]
struct Histogram {
    /// Observations at most each bound, not cumulative.
    buckets: [u64; BUCKETS.len()],
    count: u64,
    sum: f64,
}

#[derive(Default)]
struct HttpStats {
    codes: BTreeMap<u16, u64>,
    duration: Histogram,
}

/// Keyed by server so that lookups by `&str` do not allocate.
#[derive(Default)]
struct Counters {
    http: BTreeMap<String, HttpStats>,
    l4: BTreeMap<String, BTreeMap<&'static str, u64>>,
}

/// Entry of `map` for `key`, allocating the key only the first time.
fn entry<'a, V: Default>(map: &'a mut BTreeMap<String, V>, key: &str) -> &'a mut V {
    if !map.contains_key(key) {
        map.insert(key.to_owned(), V::default());
    }
    map.get_mut(key).expect("inserted above")
}

/// Process wide metrics.
pub(crate) struct Metrics {
    // ponytail: one mutex for all counters, shard per server if contention shows up.
    counters: Mutex<Counters>,
}

/// The metrics of this process.
pub(crate) fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(|| Metrics {
        counters: Mutex::default(),
    })
}

impl Metrics {
    fn lock(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Counts a finished HTTP request.
    pub(crate) fn record_http(&self, server: &str, status: u16, duration: Duration) {
        let mut counters = self.lock();
        let stats = entry(&mut counters.http, server);
        *stats.codes.entry(status).or_default() += 1;

        let seconds = duration.as_secs_f64();
        let histogram = &mut stats.duration;
        if let Some(bucket) = BUCKETS.iter().position(|bound| seconds <= *bound) {
            histogram.buckets[bucket] += 1;
        }
        histogram.count += 1;
        histogram.sum += seconds;
    }

    /// Counts an accepted layer 4 connection or UDP session.
    pub(crate) fn record_l4(&self, server: &str, protocol: &'static str) {
        *entry(&mut self.lock().l4, server)
            .entry(protocol)
            .or_default() += 1;
    }

    /// Renders every metric for the pools of `registry`.
    pub(crate) fn render(&self, registry: &Registry) -> String {
        let mut out = String::new();
        let counters = self.lock();

        header(&mut out, "rxh_build_info", "Build information.", "gauge");
        let _ = writeln!(
            out,
            "rxh_build_info{{version=\"{}\"}} 1",
            escape(crate::VERSION)
        );

        header(
            &mut out,
            "rxh_http_requests_total",
            "HTTP requests served.",
            "counter",
        );
        for (server, stats) in &counters.http {
            for (code, count) in &stats.codes {
                let _ = writeln!(
                    out,
                    "rxh_http_requests_total{{server=\"{}\",code=\"{code}\"}} {count}",
                    escape(server)
                );
            }
        }

        header(
            &mut out,
            "rxh_http_request_duration_seconds",
            "Time to send the response headers.",
            "histogram",
        );
        for (server, stats) in &counters.http {
            let histogram = &stats.duration;
            let server = escape(server);
            let mut cumulative = 0;
            for (bound, count) in BUCKETS.iter().zip(histogram.buckets) {
                cumulative += count;
                let _ = writeln!(
                    out,
                    "rxh_http_request_duration_seconds_bucket{{server=\"{server}\",le=\"{bound}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "rxh_http_request_duration_seconds_bucket{{server=\"{server}\",le=\"+Inf\"}} {}",
                histogram.count
            );
            let _ = writeln!(
                out,
                "rxh_http_request_duration_seconds_sum{{server=\"{server}\"}} {}",
                histogram.sum
            );
            let _ = writeln!(
                out,
                "rxh_http_request_duration_seconds_count{{server=\"{server}\"}} {}",
                histogram.count
            );
        }

        header(
            &mut out,
            "rxh_l4_connections_total",
            "Layer 4 connections and UDP sessions accepted.",
            "counter",
        );
        for (server, protocols) in &counters.l4 {
            for (protocol, count) in protocols {
                let _ = writeln!(
                    out,
                    "rxh_l4_connections_total{{server=\"{}\",protocol=\"{protocol}\"}} {count}",
                    escape(server)
                );
            }
        }
        drop(counters);

        let upstreams: Vec<_> = registry
            .groups()
            .flat_map(|group| {
                let pool = group.pool.name().to_owned();
                group
                    .pool
                    .upstreams()
                    .into_iter()
                    .map(move |upstream| (pool.clone(), upstream))
            })
            .collect();

        // Counts print as integers: `Display` of a whole `f64` has no decimals.
        type Value = fn(&rxh_core::pool::Upstream) -> f64;
        let families: [(&str, &str, &str, Value); 7] = [
            (
                "rxh_upstream_requests_total",
                "Requests and connections sent to the upstream.",
                "counter",
                |upstream| upstream.total_requests() as f64,
            ),
            (
                "rxh_upstream_failures_total",
                "Requests and connections to the upstream that failed without a response.",
                "counter",
                |upstream| upstream.total_failures() as f64,
            ),
            (
                "rxh_upstream_responses_total",
                "HTTP responses received from the upstream.",
                "counter",
                |upstream| upstream.total_responses() as f64,
            ),
            (
                "rxh_upstream_server_errors_total",
                "HTTP responses with a 5xx status received from the upstream.",
                "counter",
                |upstream| upstream.total_server_errors() as f64,
            ),
            (
                "rxh_upstream_response_seconds_total",
                "Time to the response headers of the upstream, summed over its responses.",
                "counter",
                |upstream| upstream.total_response_time().as_secs_f64(),
            ),
            (
                "rxh_upstream_active_requests",
                "Requests and connections in flight.",
                "gauge",
                |upstream| upstream.active_requests() as f64,
            ),
            (
                "rxh_upstream_healthy",
                "1 if the upstream is healthy, 0 otherwise.",
                "gauge",
                |upstream| f64::from(u8::from(upstream.is_healthy())),
            ),
        ];
        for (name, help, kind, value) in families {
            header(&mut out, name, help, kind);
            for (pool, upstream) in &upstreams {
                let _ = writeln!(
                    out,
                    "{name}{{pool=\"{}\",upstream=\"{}\"}} {}",
                    escape(pool),
                    escape(upstream.address()),
                    value(upstream)
                );
            }
        }

        out
    }
}

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

/// Escapes a label value: backslash, double quote and newline.
fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_label_values() {
        assert_eq!(escape("a\\b\"c\nd"), "a\\\\b\\\"c\\nd");
    }

    #[test]
    fn histogram_is_cumulative() {
        let metrics = Metrics {
            counters: Mutex::default(),
        };
        metrics.record_http("s", 200, Duration::from_millis(7));
        metrics.record_http("s", 500, Duration::from_secs(20));
        let registry = Registry::build(&rxh_core::config::Config::default()).expect("empty");
        let text = metrics.render(&registry);
        assert!(text.contains("le=\"0.005\"} 0"));
        assert!(text.contains("le=\"0.01\"} 1"));
        assert!(text.contains("le=\"10\"} 1"));
        assert!(text.contains("le=\"+Inf\"} 2"));
        assert!(text.contains("code=\"500\"} 1"));
    }
}
