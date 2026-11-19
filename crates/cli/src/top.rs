//! `rxh top`: live requests per second, latency and errors per pool, computed
//! from two scrapes of the Prometheus metrics of the admin API.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{self, IsTerminal, Write as _};
use std::time::{Duration, Instant};

use crate::client::AdminClient;

/// Clears the terminal and moves the cursor to the top left corner.
const CLEAR: &str = "\x1b[H\x1b[2J";

/// Counters of one pool, summed over its upstreams.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct PoolSample {
    requests: f64,
    failures: f64,
    responses: f64,
    server_errors: f64,
    response_seconds: f64,
    active: f64,
    healthy: u32,
    upstreams: u32,
}

/// Prints a table every `interval`, `count` times or until interrupted.
pub(crate) async fn run(
    client: &AdminClient,
    interval: Duration,
    count: Option<u32>,
) -> Result<(), String> {
    let terminal = io::stdout().is_terminal();
    let scrape = async || -> Result<_, String> {
        let text = client
            .get_text("/metrics")
            .await
            .map_err(|err| err.to_string())?;
        Ok((Instant::now(), parse(&text)))
    };

    let (mut then, mut previous) = scrape().await?;
    let mut frames: u32 = 0;
    while count.is_none_or(|count| frames < count) {
        tokio::time::sleep(interval).await;
        let (now, current) = scrape().await?;
        let mut frame = String::new();
        if terminal {
            frame.push_str(CLEAR);
        } else if frames > 0 {
            frame.push('\n');
        }
        let _ = writeln!(
            frame,
            "rxh top  admin {}  every {}",
            client.address(),
            rxh_core::duration::format(interval)
        );
        frame.push_str(&render(&previous, &current, now - then));
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(frame.as_bytes())
            .and_then(|()| stdout.flush())
            .map_err(|err| format!("cannot write the table: {err}"))?;
        (then, previous) = (now, current);
        frames += 1;
    }
    Ok(())
}

/// Sums the `rxh_upstream_*` samples of a metrics page by pool.
fn parse(text: &str) -> BTreeMap<String, PoolSample> {
    let mut pools: BTreeMap<String, PoolSample> = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("rxh_upstream_") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once('{') else {
            continue;
        };
        let Some((pool, rest)) = pool_label(rest) else {
            continue;
        };
        let Some(value) = rest
            .rsplit_once(' ')
            .and_then(|(_, value)| value.parse::<f64>().ok())
        else {
            continue;
        };
        let sample = pools.entry(pool).or_default();
        match name {
            "requests_total" => sample.requests += value,
            "failures_total" => sample.failures += value,
            "responses_total" => sample.responses += value,
            "server_errors_total" => sample.server_errors += value,
            "response_seconds_total" => sample.response_seconds += value,
            "active_requests" => sample.active += value,
            "healthy" => {
                sample.upstreams += 1;
                sample.healthy += u32::from(value > 0.0);
            }
            _ => {}
        }
    }
    pools
}

/// The `pool` label among the labels at the start of `labels` (after the
/// opening brace), unescaped, and the rest of the line after the closing brace.
fn pool_label(mut labels: &str) -> Option<(String, &str)> {
    let mut pool = None;
    loop {
        if let Some(rest) = labels.strip_prefix('}') {
            return pool.map(|pool| (pool, rest));
        }
        let (key, rest) = labels.split_once("=\"")?;
        let key = key.trim_start_matches(',');
        let mut value = String::new();
        let mut characters = rest.char_indices();
        let end = loop {
            match characters.next()? {
                (index, '"') => break index,
                (_, '\\') => match characters.next()?.1 {
                    'n' => value.push('\n'),
                    other => value.push(other),
                },
                (_, other) => value.push(other),
            }
        };
        if key == "pool" {
            pool = Some(value);
        }
        labels = &rest[end + 1..];
    }
}

/// The table of rates between two scrapes `elapsed` apart.
fn render(
    previous: &BTreeMap<String, PoolSample>,
    current: &BTreeMap<String, PoolSample>,
    elapsed: Duration,
) -> String {
    let seconds = elapsed.as_secs_f64().max(f64::EPSILON);
    let mut out = format!(
        "{:<24} {:>9} {:>9} {:>7} {:>10} {:>7} {:>8}\n",
        "POOL", "REQ/S", "ERR/S", "ERR%", "LATENCY", "ACTIVE", "HEALTHY"
    );
    if current.is_empty() {
        out.push_str("(no pools)\n");
    }
    let zero = PoolSample::default();
    for (name, now) in current {
        let before = previous.get(name).unwrap_or(&zero);
        // A counter that went down was reset (the pool was rebuilt by a reload).
        let delta = |now: f64, before: f64| if now < before { now } else { now - before };
        let requests = delta(now.requests, before.requests);
        let responses = delta(now.responses, before.responses);
        let failures = delta(now.failures, before.failures);
        let errors = failures + delta(now.server_errors, before.server_errors);
        // Requests are counted when they start and errors when they end, so
        // HTTP pools compare errors with the requests that ended in the window.
        let finished = if responses > 0.0 {
            responses + failures
        } else {
            requests
        };
        let error_share = if finished > 0.0 {
            format!("{:.1}", 100.0 * errors.min(finished) / finished)
        } else {
            String::from("-")
        };
        let latency = if responses > 0.0 {
            let average = delta(now.response_seconds, before.response_seconds) / responses;
            format!("{:.1}ms", average * 1000.0)
        } else {
            String::from("-")
        };
        let _ = writeln!(
            out,
            "{name:<24} {:>9.1} {:>9.1} {error_share:>7} {latency:>10} {:>7} {:>8}",
            requests / seconds,
            errors / seconds,
            now.active,
            format!("{}/{}", now.healthy, now.upstreams),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"# HELP rxh_upstream_requests_total Requests.
# TYPE rxh_upstream_requests_total counter
rxh_http_requests_total{server="web",code="200"} 9
rxh_upstream_requests_total{pool="api",upstream="10.0.0.1:80"} 10
rxh_upstream_requests_total{pool="api",upstream="10.0.0.2:80"} 30
rxh_upstream_requests_total{pool="we\"ird\\",upstream="a:1"} 5
rxh_upstream_failures_total{pool="api",upstream="10.0.0.1:80"} 2
rxh_upstream_responses_total{pool="api",upstream="10.0.0.1:80"} 8
rxh_upstream_responses_total{pool="api",upstream="10.0.0.2:80"} 30
rxh_upstream_server_errors_total{pool="api",upstream="10.0.0.2:80"} 1
rxh_upstream_response_seconds_total{pool="api",upstream="10.0.0.1:80"} 0.38
rxh_upstream_active_requests{pool="api",upstream="10.0.0.1:80"} 3
rxh_upstream_healthy{pool="api",upstream="10.0.0.1:80"} 1
rxh_upstream_healthy{pool="api",upstream="10.0.0.2:80"} 0
"#;

    #[test]
    fn parse_sums_upstreams_by_pool() {
        let pools = parse(PAGE);
        assert_eq!(pools.len(), 2, "{pools:?}");
        assert_eq!(
            pools["api"],
            PoolSample {
                requests: 40.0,
                failures: 2.0,
                responses: 38.0,
                server_errors: 1.0,
                response_seconds: 0.38,
                active: 3.0,
                healthy: 1,
                upstreams: 2,
            }
        );
        assert_eq!(pools["we\"ird\\"].requests, 5.0);
    }

    #[test]
    fn pool_label_is_found_after_other_labels() {
        assert_eq!(
            pool_label(r#"upstream="a,\"b",pool="p\nq"} 7"#),
            Some((String::from("p\nq"), " 7"))
        );
        assert_eq!(pool_label(r#"upstream="a"} 7"#), None);
        assert_eq!(pool_label(r#"pool="unterminated"#), None);
    }

    fn row<'a>(table: &'a str, pool: &str) -> Vec<&'a str> {
        table
            .lines()
            .find(|line| line.starts_with(pool))
            .unwrap_or_else(|| panic!("no row for {pool} in\n{table}"))
            .split_whitespace()
            .collect()
    }

    #[test]
    fn render_computes_rates_from_deltas() {
        let before = BTreeMap::from([(
            String::from("api"),
            PoolSample {
                requests: 100.0,
                failures: 1.0,
                responses: 99.0,
                server_errors: 0.0,
                response_seconds: 5.0,
                active: 0.0,
                healthy: 2,
                upstreams: 2,
            },
        )]);
        let after = BTreeMap::from([
            (
                String::from("api"),
                PoolSample {
                    requests: 140.0,
                    failures: 3.0,
                    responses: 137.0,
                    server_errors: 2.0,
                    response_seconds: 5.76,
                    active: 4.0,
                    healthy: 1,
                    upstreams: 2,
                },
            ),
            (
                String::from("idle"),
                PoolSample {
                    healthy: 1,
                    upstreams: 1,
                    ..PoolSample::default()
                },
            ),
        ]);
        let table = render(&before, &after, Duration::from_secs(2));
        // 40 requests and 2 failures + 2 5xx in 2 s; 0.76 s over 38 responses.
        assert_eq!(
            row(&table, "api"),
            ["api", "20.0", "2.0", "10.0", "20.0ms", "4", "1/2"]
        );
        assert_eq!(
            row(&table, "idle"),
            ["idle", "0.0", "0.0", "-", "-", "0", "1/1"]
        );
    }

    #[test]
    fn render_treats_a_decrease_as_a_reset() {
        let sample = |requests: f64| {
            BTreeMap::from([(
                String::from("api"),
                PoolSample {
                    requests,
                    ..PoolSample::default()
                },
            )])
        };
        let table = render(&sample(500.0), &sample(6.0), Duration::from_secs(1));
        assert_eq!(row(&table, "api")[1], "6.0");
        assert!(
            render(&sample(1.0), &BTreeMap::new(), Duration::from_secs(1)).contains("(no pools)")
        );
    }
}
