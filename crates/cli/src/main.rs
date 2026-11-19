//! `rxh` binary: runs the proxy daemon and manages it from the command line.

mod cli;
mod client;
mod commands;
mod edit;
mod process;
mod quick;
mod top;

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use rxh_core::config::{AdminListen, BasicAuth, Config};

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
        Some(Command::HashPassword) => hash_password(),
        Some(Command::Completions { shell }) => completions(shell),
        Some(Command::Man { dir }) => man(dir.as_deref()),
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

/// `rxh hash-password`: prints the bcrypt hash of the first line of standard
/// input.
fn hash_password() -> Result<(), String> {
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| format!("cannot read the password: {err}"))?;
    let password = line.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Err(String::from("no password on standard input"));
    }
    println!("{}", BasicAuth::hash_password(password)?);
    Ok(())
}

/// `rxh completions`: prints the completion script of `shell`.
fn completions(shell: clap_complete::Shell) -> Result<(), String> {
    use std::io::Write;

    // `generate` panics on write errors (a closed pipe), so render to memory.
    let mut script: Vec<u8> = Vec::new();
    clap_complete::generate(shell, &mut Cli::command(), "rxh", &mut script);
    io::stdout()
        .write_all(&script)
        .map_err(|err| format!("cannot write the completions: {err}"))
}

/// `rxh man`: prints `rxh.1`, or writes every page into `dir`.
fn man(dir: Option<&Path>) -> Result<(), String> {
    let mut command = Cli::command().disable_help_subcommand(true);
    command.build();
    let result = match dir {
        None => man_page(command).render(&mut io::stdout()),
        Some(dir) => std::fs::create_dir_all(dir).and_then(|()| write_man_pages(command, dir)),
    };
    result.map_err(|err| format!("cannot write the man pages: {err}"))
}

/// Writes the page of `command` and of each of its subcommands into `dir`.
fn write_man_pages(command: clap::Command, dir: &Path) -> io::Result<()> {
    for subcommand in command.get_subcommands() {
        write_man_pages(subcommand.clone(), dir)?;
    }
    man_page(command).generate_to(dir).map(|_| ())
}

/// The page of a built command, with the `rxh VERSION` footer that clap_mangen
/// would otherwise leave as the bare subcommand name.
fn man_page(command: clap::Command) -> clap_mangen::Man {
    clap_mangen::Man::new(command).source(format!("rxh {}", rxh_server::VERSION))
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
        let busy = match &config.admin.listen {
            AdminListen::Tcp(address) => TcpListener::bind(address).err(),
            #[cfg(unix)]
            AdminListen::Unix(path) => std::os::unix::net::UnixStream::connect(path)
                .ok()
                .map(|_| io::Error::from(io::ErrorKind::AddrInUse)),
            #[cfg(not(unix))]
            AdminListen::Unix(_) => None,
        };
        if let Some(err) = busy {
            return Err(format!(
                "admin address {} is busy ({err}), is rxh already running?",
                config.admin.listen
            ));
        }
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
