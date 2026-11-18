//! `rxh-bench`: runs the same load balancing scenarios against rxh and Caddy
//! and reports correctness (PASS/FAIL) and performance side by side.

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
use crate::target::{Launcher, Target};

/// Compares rxh and Caddy on load balancing behaviour and speed.
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

    /// Scenario to run. Repeat for several. Defaults to all.
    #[arg(long = "scenario", short, value_enum, value_name = "NAME")]
    scenarios: Vec<Scenario>,

    /// Load duration per scenario, in seconds.
    #[arg(long, short, value_name = "SECONDS")]
    duration: Option<f64>,

    /// Parallel client connections.
    #[arg(long, short, value_name = "N")]
    concurrency: Option<usize>,

    /// Short runs (2s, 16 connections), for CI and smoke tests.
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
            Some(binary) => targets.push(Target::rxh(binary)),
            None => missing.push((
                "rxh",
                String::from("binary not found, run `cargo build --release -p rxh-cli`"),
            )),
        }
    }
    if !cli.no_caddy {
        match caddy_launcher(&cli) {
            Ok(launcher) => targets.push(Target::caddy(launcher, cli.caddy_l4.clone())),
            Err(reason) => missing.push(("caddy", reason)),
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

    if outcomes
        .iter()
        .any(|outcome| outcome.verdict == Verdict::Fail)
    {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Picks how to run Caddy: explicit binary, PATH, or Docker if allowed.
fn caddy_launcher(cli: &Cli) -> Result<Launcher, String> {
    if let Some(binary) = cli.caddy.clone().or_else(|| target::find_in_path("caddy")) {
        return Ok(Launcher::Binary(binary));
    }
    if !cli.caddy_docker {
        return Err(String::from(
            "caddy not found, pass --caddy PATH or --caddy-docker",
        ));
    }

    let docker = |args: &[&str]| {
        std::process::Command::new("docker")
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    if !docker(&["image", "inspect", &cli.caddy_image]) {
        eprintln!("pulling {}...", cli.caddy_image);
        if !docker(&["pull", &cli.caddy_image]) {
            return Err(format!("docker could not pull {}", cli.caddy_image));
        }
    }
    Ok(Launcher::Docker(cli.caddy_image.clone()))
}

/// Markdown report: one row per scenario and target, then a head to head
/// comparison when both targets ran.
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

    let mut comparison = String::new();
    for rxh in outcomes.iter().filter(|outcome| outcome.target == "rxh") {
        let caddy = outcomes
            .iter()
            .find(|outcome| outcome.target == "caddy" && outcome.scenario == rxh.scenario);
        let (Some(rxh_stats), Some(caddy_stats)) =
            (&rxh.stats, caddy.and_then(|outcome| outcome.stats.as_ref()))
        else {
            continue;
        };
        let _ = writeln!(
            comparison,
            "| {} | {:.0} | {:.0} | {:.2}x | {} | {} |",
            rxh.scenario.name(),
            rxh_stats.rps,
            caddy_stats.rps,
            rxh_stats.rps / caddy_stats.rps.max(f64::EPSILON),
            millis(rxh_stats.p99_us),
            millis(caddy_stats.p99_us)
        );
    }
    if !comparison.is_empty() {
        let _ = write!(
            text,
            "\n### rxh vs caddy\n\n\
             | Scenario | rxh req/s | caddy req/s | rxh/caddy | rxh p99 | caddy p99 |\n\
             |---|---:|---:|---:|---:|---:|\n{comparison}"
        );
    }

    text
}
