//! Benchmark history: rxh's numbers across releases, read from one
//! `rxh-bench --json` file per release, with regressions marked.

use std::cmp::Ordering;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::Deserialize;

/// A change against the previous release larger than this fraction is a
/// regression. Single runs differ by up to 10% (docs/benchmarks.md).
const THRESHOLD: f64 = 0.10;

/// The part of an `rxh-bench --json` outcome the history needs. Scenarios
/// stay strings so files of releases with other scenarios still load.
#[derive(Debug, Deserialize)]
struct Outcome {
    scenario: String,
    target: String,
    verdict: String,
    stats: Option<Stats>,
}

#[derive(Debug, Deserialize)]
struct Stats {
    rps: f64,
    p99_us: u32,
}

/// The rxh outcomes of one release.
#[derive(Debug)]
struct Release {
    version: String,
    /// CPU from the `- CPU: ` line of `<version>.md`, when there is one.
    cpu: Option<String>,
    outcomes: Vec<Outcome>,
}

impl Release {
    fn outcome(&self, scenario: &str) -> Option<&Outcome> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.scenario == scenario)
    }
}

/// One measured value, with the direction in which it gets worse.
#[derive(Debug, Clone, Copy)]
enum Metric {
    Rps,
    P99,
}

impl Metric {
    fn value(self, stats: &Stats) -> f64 {
        match self {
            Metric::Rps => stats.rps,
            Metric::P99 => f64::from(stats.p99_us) / 1000.0,
        }
    }

    fn format(self, value: f64) -> String {
        match self {
            Metric::Rps => format!("{value:.0}"),
            Metric::P99 => format!("{value:.2}"),
        }
    }

    /// Relative change that counts as worse: fewer requests, higher latency.
    fn worse(self, change: f64) -> bool {
        match self {
            Metric::Rps => change < -THRESHOLD,
            Metric::P99 => change > THRESHOLD,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Metric::Rps => "req/s",
            Metric::P99 => "p99",
        }
    }
}

/// Reads every `<version>.json` in `dir` and renders the history as
/// Markdown, oldest release first.
pub fn render_dir(dir: &Path) -> Result<String, String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut releases = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| format!("{}: {err}", dir.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(version) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let outcomes: Vec<Outcome> =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        let cpu = fs::read_to_string(path.with_extension("md"))
            .ok()
            .and_then(|notes| {
                let line = notes
                    .lines()
                    .find_map(|line| line.strip_prefix("- CPU: "))?;
                Some(line.trim().to_owned())
            });
        releases.push(Release {
            version: version.to_owned(),
            cpu,
            outcomes: outcomes
                .into_iter()
                .filter(|outcome| outcome.target == "rxh")
                .collect(),
        });
    }
    if releases.is_empty() {
        return Err(format!("no <version>.json file in {}", dir.display()));
    }
    releases.sort_by(|a, b| compare_versions(&a.version, &b.version));
    Ok(render(&releases))
}

/// Orders `0.9.0` before `0.10.0`, and `0.3.0-rc1` before `0.3.0`.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let key = |version: &str| {
        let (release, pre) = match version.split_once('-') {
            Some((release, pre)) => (release, Some(pre)),
            None => (version, None),
        };
        let numbers: Vec<u64> = release
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect();
        (numbers, pre.is_none(), pre.map(str::to_owned))
    };
    key(a).cmp(&key(b))
}

fn render(releases: &[Release]) -> String {
    let mut scenarios: Vec<&str> = Vec::new();
    for release in releases {
        for outcome in &release.outcomes {
            if !scenarios.contains(&outcome.scenario.as_str()) {
                scenarios.push(&outcome.scenario);
            }
        }
    }

    let versions: Vec<&str> = releases
        .iter()
        .map(|release| release.version.as_str())
        .collect();
    let mut text = format!(
        "## rxh benchmark history\n\n\
         rxh's numbers in `rxh-bench --docker`, one run per release. A change of \
         more than {:.0}% against the previous release is in bold and listed under \
         Regressions. Verdicts other than PASS are shown next to the number.\n",
        THRESHOLD * 100.0
    );
    text.push('\n');
    for release in releases {
        let cpu = release.cpu.as_deref().unwrap_or("unknown CPU");
        let _ = writeln!(text, "- {}: {cpu}", release.version);
    }
    let mut cpus: Vec<Option<&str>> = releases
        .iter()
        .map(|release| release.cpu.as_deref())
        .collect();
    cpus.dedup();
    if cpus.len() > 1 {
        text.push_str(
            "\n**Warning**: not every release ran on the same CPU. Compare only releases \
             measured on the same machine.\n",
        );
    }
    let mut regressions = Vec::new();
    for (metric, title) in [
        (Metric::Rps, "Requests per second"),
        (Metric::P99, "p99 latency in milliseconds"),
    ] {
        let _ = write!(
            text,
            "\n### {title}\n\n| Scenario | {} |\n|---|{}\n",
            versions.join(" | "),
            "---:|".repeat(versions.len())
        );
        for scenario in &scenarios {
            let _ = write!(text, "| {scenario} |");
            let mut previous: Option<f64> = None;
            for release in releases {
                let outcome = release.outcome(scenario);
                let value = outcome
                    .and_then(|outcome| outcome.stats.as_ref())
                    .map(|stats| metric.value(stats));
                let mut cell = match value {
                    Some(value) => metric.format(value),
                    None => String::from("-"),
                };
                if let (Some(value), Some(before)) = (value, previous)
                    && before > 0.0
                {
                    let change = value / before - 1.0;
                    let _ = write!(cell, " ({:+.0}%)", change * 100.0);
                    if metric.worse(change) {
                        cell = format!("**{cell}**");
                        regressions.push(format!(
                            "- {} `{scenario}`: {} {} to {} ({:+.0}%)",
                            release.version,
                            metric.label(),
                            metric.format(before),
                            metric.format(value),
                            change * 100.0
                        ));
                    }
                }
                if let Some(outcome) = outcome
                    && outcome.verdict != "pass"
                {
                    let _ = write!(cell, " {}", outcome.verdict.to_uppercase());
                }
                let _ = write!(text, " {cell} |");
                // A release without the scenario does not break the chain.
                previous = value.or(previous);
            }
            text.push('\n');
        }
    }

    text.push_str("\n### Regressions\n\n");
    if regressions.is_empty() {
        let _ = writeln!(
            text,
            "None above {:.0}% against the previous release.",
            THRESHOLD * 100.0
        );
    } else {
        for regression in regressions {
            let _ = writeln!(text, "{regression}");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn release(version: &str, outcomes: &[(&str, &str, f64, u32)]) -> Release {
        Release {
            version: version.to_owned(),
            cpu: Some(String::from("Test CPU")),
            outcomes: outcomes
                .iter()
                .map(|&(scenario, verdict, rps, p99_us)| Outcome {
                    scenario: scenario.to_owned(),
                    target: String::from("rxh"),
                    verdict: verdict.to_owned(),
                    stats: Some(Stats { rps, p99_us }),
                })
                .collect(),
        }
    }

    #[test]
    fn versions_sort_numerically_with_pre_releases_first() {
        let mut versions = ["0.10.0", "0.3.0", "0.9.1", "0.3.0-rc1", "1.0.0"];
        versions.sort_by(|a, b| compare_versions(a, b));
        assert_eq!(versions, ["0.3.0-rc1", "0.3.0", "0.9.1", "0.10.0", "1.0.0"]);
    }

    #[test]
    fn fewer_requests_and_higher_latency_are_regressions() {
        let text = render(&[
            release("0.2.0", &[("throughput", "pass", 100_000.0, 500)]),
            release("0.3.0", &[("throughput", "pass", 85_000.0, 600)]),
        ]);
        assert!(
            text.contains("| throughput | 100000 | **85000 (-15%)** |"),
            "{text}"
        );
        assert!(
            text.contains("| throughput | 0.50 | **0.60 (+20%)** |"),
            "{text}"
        );
        assert!(text.contains("- 0.3.0 `throughput`: req/s 100000 to 85000 (-15%)"));
        assert!(text.contains("- 0.3.0 `throughput`: p99 0.50 to 0.60 (+20%)"));
    }

    #[test]
    fn improvements_and_noise_are_not_regressions() {
        let text = render(&[
            release("0.2.0", &[("tcp", "pass", 100_000.0, 500)]),
            release("0.3.0", &[("tcp", "pass", 92_000.0, 300)]),
        ]);
        assert!(text.contains("| tcp | 100000 | 92000 (-8%) |"), "{text}");
        assert!(text.contains("| tcp | 0.50 | 0.30 (-40%) |"), "{text}");
        assert!(text.contains("None above 10%"), "{text}");
    }

    #[test]
    fn missing_scenarios_and_failures_are_shown() {
        let text = render(&[
            release("0.2.0", &[("throughput", "pass", 100_000.0, 500)]),
            release("0.3.0", &[("udp", "pass", 50_000.0, 200)]),
            release("0.4.0", &[("throughput", "fail", 100_000.0, 500)]),
        ]);
        // 0.4.0 compares with 0.2.0, the last release that ran the scenario.
        assert!(
            text.contains("| throughput | 100000 | - | 100000 (+0%) FAIL |"),
            "{text}"
        );
        assert!(text.contains("| udp | - | 50000 | - |"), "{text}");
    }

    #[test]
    fn different_machines_are_flagged() {
        let same = render(&[release("0.2.0", &[]), release("0.3.0", &[])]);
        assert!(same.contains("- 0.3.0: Test CPU"), "{same}");
        assert!(!same.contains("Warning"), "{same}");

        let mut other = release("0.3.0", &[]);
        other.cpu = Some(String::from("Other CPU"));
        let different = render(&[release("0.2.0", &[]), other]);
        assert!(
            different.contains("not every release ran on the same CPU"),
            "{different}"
        );
    }

    #[test]
    fn empty_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(render_dir(dir.path()).is_err());
    }

    #[test]
    fn committed_history_loads() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/bench/history");
        let text = render_dir(&dir).unwrap();
        assert!(text.contains("| throughput | 138548 |"), "{text}");
        assert!(
            text.contains("- 0.2.0: Intel(R) Core(TM) Ultra 7 255H"),
            "{text}"
        );
    }
}
