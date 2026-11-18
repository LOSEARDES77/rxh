//! `rxh` binary: runs the proxy daemon and manages it from the command line.

mod cli;
mod client;
mod commands;
mod edit;
mod process;
mod quick;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use rxh_core::config::Config;

use crate::cli::{Cli, Command};

fn main() -> ExitCode {
    let mut cli = Cli::parse();

    let result = match cli.command.take() {
        None | Some(Command::Run) => run(&cli),
        Some(Command::Start { log_file }) => start(&cli, log_file),
        Some(Command::FileServer {
            listen,
            root,
            no_browse,
            encode,
        }) => file_server(listen, &root, !no_browse, encode),
        Some(Command::ReverseProxy {
            from,
            to,
            policy,
            protocol,
            host,
        }) => {
            quick::reverse_proxy(from, &to, &policy, protocol, host.as_deref()).and_then(|config| {
                println!("Proxying {from} to {}", to.join(", "));
                run_config(config)
            })
        }
        Some(command) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| err.to_string())
            .and_then(|runtime| runtime.block_on(commands::execute(&cli, command))),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("rxh: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the daemon in the foreground.
fn run(cli: &Cli) -> Result<(), String> {
    let path = commands::config_path(cli);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;
    runtime
        .block_on(rxh_server::Daemon::new(path).run())
        .map_err(|err| err.to_string())
}

/// `rxh file-server`: serves a directory without a configuration file.
fn file_server(listen: SocketAddr, root: &Path, browse: bool, encode: bool) -> Result<(), String> {
    let root = std::fs::canonicalize(root).map_err(|err| format!("{}: {err}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let config = quick::file_server(listen, &root, browse, encode)?;
    // Fail with a clear message now if the address is taken; the daemon
    // binds it again right after.
    TcpListener::bind(listen).map_err(|err| format!("cannot listen on {listen}: {err}"))?;
    println!("Serving {} on http://{listen}", root.display());
    run_config(config)
}

/// Runs a configuration that has no file in the foreground.
fn run_config(config: Config) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| err.to_string())?;
    runtime
        .block_on(rxh_server::Daemon::from_config(config).run())
        .map_err(|err| err.to_string())
}

/// Checks the configuration, forks into the background and runs the daemon.
fn start(cli: &Cli, log_file: Option<PathBuf>) -> Result<(), String> {
    let path = commands::config_path(cli);
    let config = rxh_server::load_config(&path).map_err(|err| err.to_string())?;

    // Fail in the foreground, where the user can see it, if another daemon
    // already owns the admin address.
    if config.admin.enabled {
        TcpListener::bind(config.admin.listen).map_err(|err| {
            format!(
                "admin address {} is busy ({err}), is rxh already running?",
                config.admin.listen
            )
        })?;
    }

    let log_file = log_file.unwrap_or_else(|| cli.pid_file.with_extension("log"));
    println!(
        "Starting rxh in the background (pid file {}, log {})",
        cli.pid_file.display(),
        log_file.display()
    );
    process::daemonize(&cli.pid_file, &log_file).map_err(|err| err.to_string())?;

    run(cli)
}
