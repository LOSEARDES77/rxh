//! `rxh-bench`: runs the same load balancing scenarios against rxh, Caddy,
//! NGINX and Traefik and reports correctness (PASS/FAIL) and performance side by side.

mod backend;
mod load;
mod scenario;
mod target;

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use clap::{Parser, ValueEnum};

use crate::scenario::{Options, Outcome, Scenario, Verdict};
use crate::target::{Flavor, Launcher, Target};

/// Compares rxh with Caddy, NGINX and Traefik on load balancing behaviour and speed.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// rxh binary. Defaults to the one next to this tool, target/release,
    /// target/debug, then PATH.
    #[arg(long, value_name = "PATH")]
    rxh: Option<PathBuf>,

    /// Skip rxh.
    #[arg(long)]
    no_rxh: bool,

    /// Caddy binary. Defaults to `caddy` from PATH.
    #[arg(long, value_name = "PATH")]
    caddy: Option<PathBuf>,

    /// Run Caddy from a Docker image when no binary is available.
    #[arg(long)]
    caddy_docker: bool,

    /// Image used by --caddy-docker.
    #[arg(long, default_value = "caddy:2", value_name = "IMAGE")]
    caddy_image: String,

    /// Caddy binary built with the caddy-l4 plugin, for the tcp scenario.
    #[arg(long, value_name = "PATH")]
    caddy_l4: Option<PathBuf>,

    /// Skip Caddy.
    #[arg(long)]
    no_caddy: bool,

    /// Run every proxy in Docker with host networking, and add NGINX and
    /// Traefik. The local rxh binary is mounted into --rxh-image.
    #[arg(long)]
    docker: bool,

    /// Base image the rxh binary runs in with --docker.
    #[arg(long, default_value = "debian:stable-slim", value_name = "IMAGE")]
    rxh_image: String,

    /// NGINX image used by --docker.
    #[arg(long, default_value = "nginx:stable-alpine", value_name = "IMAGE")]
    nginx_image: String,

    /// Skip NGINX.
    #[arg(long)]
    no_nginx: bool,

    /// Traefik image used by --docker.
    #[arg(long, default_value = "traefik:v3", value_name = "IMAGE")]
    traefik_image: String,

    /// Skip Traefik.
    #[arg(long)]
    no_traefik: bool,

    /// Scenario to run. Repeat for several. Defaults to all.
    #[arg(long = "scenario", short, value_enum, value_name = "NAME")]
    scenarios: Vec<Scenario>,

    /// Load duration per scenario, in seconds.
    #[arg(long, short, value_name = "SECONDS")]
    duration: Option<f64>,

    /// Parallel client connections.
    #[arg(long, short, value_name = "N")]
    concurrency: Option<usize>,

    /// Idle keep-alive connections held open by the idle and memory
    /// scenarios.
    #[arg(long, value_name = "N")]
    idle_connections: Option<usize>,

    /// Short runs (2s, 16 connections, 1000 idle connections), for CI and
    /// smoke tests.
    #[arg(long)]
    quick: bool,

    /// Also write the results as JSON.
    #[arg(long, value_name = "FILE")]
    json: Option<PathBuf>,

    /// List scenarios and exit.
    #[arg(long)]
    list: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Dropping `run` on Ctrl-C kills every proxy it started.
    tokio::select! {
        code = run(cli) => code,
        _ = tokio::signal::ctrl_c() => {
            eprintln!("interrupted, stopping proxies");
            ExitCode::from(130)
        }
    }
}

async fn run(cli: Cli) -> ExitCode {
    if cli.list {
        for scenario in Scenario::value_variants() {
            let help = scenario
                .to_possible_value()
                .and_then(|value| value.get_help().map(ToString::to_string))
                .unwrap_or_default();
            println!("{:<14} {help}", scenario.name());
        }
        return ExitCode::SUCCESS;
    }

    let options = Options {
        duration: Duration::from_secs_f64(
            cli.duration
                .unwrap_or(if cli.quick { 2.0 } else { 5.0 })
                .max(0.5),
        ),
        concurrency: cli
            .concurrency
            .unwrap_or(if cli.quick { 16 } else { 32 })
            .max(1),
        idle_connections: cli
            .idle_connections
            .unwrap_or(if cli.quick { 1000 } else { 5000 })
            .max(1),
    };
    let scenarios = if cli.scenarios.is_empty() {
        Scenario::value_variants().to_vec()
    } else {
        cli.scenarios.clone()
    };

    let mut targets = Vec::new();
    let mut missing = Vec::new();
    if !cli.no_rxh {
        match cli.rxh.clone().or_else(target::find_rxh) {
            Some(binary) => match rxh_launcher(&cli, binary) {
                Ok(launcher) => {
                    targets.push(Target::new(Flavor::Rxh, launcher.clone(), Some(launcher)));
                }
                Err(reason) => missing.push(("rxh", reason)),
            },
            None => missing.push((
                "rxh",
                String::from("binary not found, run `cargo build --release -p rxh-cli`"),
            )),
        }
    }
    if !cli.no_caddy {
        match caddy_launcher(&cli) {
            Ok(launcher) => targets.push(Target::new(
                Flavor::Caddy,
                launcher,
                cli.caddy_l4.clone().map(Launcher::Binary),
            )),
            Err(reason) => missing.push(("caddy", reason)),
        }
    }
    if cli.docker {
        let extras = [
            (Flavor::Nginx, &cli.nginx_image, cli.no_nginx),
            (Flavor::Traefik, &cli.traefik_image, cli.no_traefik),
        ];
        for (flavor, image, skip) in extras {
            if skip {
                continue;
            }
            match docker_image(image) {
                Ok(launcher) => {
                    targets.push(Target::new(flavor, launcher.clone(), Some(launcher)));
                }
                Err(reason) => missing.push((flavor.name(), reason)),
            }
        }
    }

    let mut outcomes = Vec::new();
    for scenario in &scenarios {
        for target in &targets {
            eprintln!("running {} on {}...", scenario.name(), target.name);
            let outcome = scenario::run(*scenario, target, options).await;
            eprintln!("  {} {}", outcome.verdict, outcome.reason);
            outcomes.push(outcome);
        }
        for (name, reason) in &missing {
            outcomes.push(Outcome {
                scenario: *scenario,
                target: (*name).to_owned(),
                verdict: Verdict::NotAvailable,
                reason: reason.clone(),
                stats: None,
                distribution: Vec::new(),
            });
        }
    }

    println!("{}", report(&outcomes, options));

    if let Some(path) = &cli.json {
        let json = serde_json::to_string_pretty(&outcomes).expect("outcomes serialize to JSON");
        if let Err(err) = fs::write(path, json) {
            eprintln!("could not write {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    }

    if rxh_failed(&outcomes) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Whether the run fails as a correctness gate. Only rxh counts: the other
/// proxies are reported for information. An rxh `n/a` counts as a failure,
/// so a missing binary or a scenario rxh cannot run does not pass silently.
fn rxh_failed(outcomes: &[Outcome]) -> bool {
    outcomes
        .iter()
        .any(|outcome| outcome.target == Flavor::Rxh.name() && outcome.verdict != Verdict::Pass)
}

/// Picks how to run rxh: the binary itself, or mounted into a container.
fn rxh_launcher(cli: &Cli, binary: PathBuf) -> Result<Launcher, String> {
    if !cli.docker {
        return Ok(Launcher::Binary(binary));
    }
    // Docker needs an absolute path to mount.
    let binary = fs::canonicalize(&binary).map_err(|err| format!("{}: {err}", binary.display()))?;
    let Launcher::Docker { image, .. } = docker_image(&cli.rxh_image)? else {
        unreachable!("docker_image returns a Docker launcher");
    };
    Ok(Launcher::Docker {
        image,
        binary: Some(binary),
    })
}

/// Picks how to run Caddy: explicit binary, PATH, or Docker if allowed.
fn caddy_launcher(cli: &Cli) -> Result<Launcher, String> {
    if cli.docker {
        return docker_image(&cli.caddy_image);
    }
    if let Some(binary) = cli.caddy.clone().or_else(|| target::find_in_path("caddy")) {
        return Ok(Launcher::Binary(binary));
    }
    if !cli.caddy_docker {
        return Err(String::from(
            "caddy not found, pass --caddy PATH or --caddy-docker",
        ));
    }
    docker_image(&cli.caddy_image)
}

/// A Docker launcher for `image`, pulling it when it is not local yet.
fn docker_image(image: &str) -> Result<Launcher, String> {
    let docker = |args: &[&str]| {
        std::process::Command::new("docker")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !docker(&["image", "inspect", image]) {
        eprintln!("pulling {image}...");
        if !docker(&["pull", image]) {
            return Err(format!("docker could not pull {image}"));
        }
    }
    Ok(Launcher::Docker {
        image: image.to_owned(),
        binary: None,
    })
}

/// Markdown report: one row per scenario and target, then a head to head
/// comparison when several targets ran.
fn report(outcomes: &[Outcome], options: Options) -> String {
    let millis = |micros: u32| format!("{:.2}ms", f64::from(micros) / 1000.0);

    let mut text = format!(
        "## rxh-bench results\n\n{:?} per scenario, {} connections\n\n\
         | Scenario | Target | Verdict | Req/s | p50 | p90 | p99 | Errors | Distribution | Notes |\n\
         |---|---|---|---:|---:|---:|---:|---:|---|---|\n",
        options.duration, options.concurrency
    );
    for outcome in outcomes {
        let numbers = match &outcome.stats {
            Some(stats) => format!(
                "{:.0} | {} | {} | {} | {}",
                stats.rps,
                millis(stats.p50_us),
                millis(stats.p90_us),
                millis(stats.p99_us),
                stats.errors
            ),
            None => String::from("- | - | - | - | -"),
        };
        let distribution: Vec<String> = outcome
            .distribution
            .iter()
            .map(ToString::to_string)
            .collect();
        let _ = writeln!(
            text,
            "| {} | {} | {} | {numbers} | {} | {} |",
            outcome.scenario.name(),
            outcome.target,
            outcome.verdict,
            distribution.join(" / "),
            outcome.reason.replace('|', "\\|")
        );
    }

    // Requests per second of every target next to rxh's.
    let mut names: Vec<&str> = Vec::new();
    for outcome in outcomes {
        if outcome.stats.is_some() && !names.contains(&outcome.target.as_str()) {
            names.push(&outcome.target);
        }
    }
    if names.len() < 2 {
        return text;
    }
    let _ = write!(
        text,
        "\n### Requests per second (and ratio to rxh)\n\n| Scenario | {} |\n|---|{}\n",
        names.join(" | "),
        "---:|".repeat(names.len())
    );
    let mut scenarios = Vec::new();
    for outcome in outcomes {
        if !scenarios.contains(&outcome.scenario) {
            scenarios.push(outcome.scenario);
        }
    }
    for scenario in scenarios {
        let rps = |name: &str| {
            let outcome = outcomes
                .iter()
                .find(|outcome| outcome.scenario == scenario && outcome.target == name)?;
            Some(outcome.stats.as_ref()?.rps)
        };
        let rxh = rps("rxh");
        let _ = write!(text, "| {} |", scenario.name());
        for name in &names {
            let cell = match (rps(name), rxh) {
                (Some(rps), Some(rxh)) if *name != "rxh" => {
                    format!("{rps:.0} ({:.2}x)", rps / rxh.max(f64::EPSILON))
                }
                (Some(rps), _) => format!("{rps:.0}"),
                (None, _) => String::from("-"),
            };
            let _ = write!(text, " {cell} |");
        }
        text.push('\n');
    }

    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(target: &str, verdict: Verdict) -> Outcome {
        Outcome {
            scenario: Scenario::Throughput,
            target: target.to_owned(),
            verdict,
            reason: String::new(),
            stats: None,
            distribution: Vec::new(),
        }
    }

    #[test]
    fn other_proxies_do_not_fail_the_gate() {
        let outcomes = [
            outcome("rxh", Verdict::Pass),
            outcome("caddy", Verdict::Fail),
            outcome("nginx", Verdict::NotAvailable),
        ];
        assert!(!rxh_failed(&outcomes));
    }

    #[test]
    fn rxh_failure_fails_the_gate() {
        let outcomes = [
            outcome("rxh", Verdict::Fail),
            outcome("caddy", Verdict::Pass),
        ];
        assert!(rxh_failed(&outcomes));
    }

    #[test]
    fn rxh_not_available_fails_the_gate() {
        assert!(rxh_failed(&[outcome("rxh", Verdict::NotAvailable)]));
    }
}
