//! `rxh` binary: runs the proxy daemon and manages it from the command line.

mod adapt;
mod cli;
mod client;
mod commands;
mod edit;
mod fmt;
mod process;
mod quick;
mod top;

use std::env;
use std::fs;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser};
use rxh_core::config::{AdminListen, BasicAuth, Config};
use rxh_core::placeholder::{self, Lookup};

use crate::cli::{Adapter, Cli, Command};

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
        Some(Command::Fmt { files, check }) => format_files(&cli, files, check),
        Some(Command::Adapt { file, adapter }) => adapt_file(&file, adapter),
        Some(Command::Schema) => schema(),
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
    // Not during a hot binary upgrade: the daemon being replaced holds it.
    if !rxh_server::is_upgrade() {
        TcpListener::bind(listen).map_err(|err| format!("cannot listen on {listen}: {err}"))?;
    }
    println!("Serving {} on http://{listen}", root.display());
    run_config(config)
}

/// `rxh hash-password`: prints the bcrypt hash of the first line of standard
/// input.
/// Formats `files`, or the configuration file. With `check`, only reports.
fn format_files(cli: &Cli, mut files: Vec<PathBuf>, check: bool) -> Result<(), String> {
    if files.is_empty() {
        files.push(commands::config_path(cli));
    }
    let mut unformatted: usize = 0;
    for path in &files {
        let text: String = fs::read_to_string(path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        let formatted: String =
            fmt::format(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        if formatted == text {
            continue;
        }
        unformatted += 1;
        if check {
            println!("{}", path.display());
        } else {
            fs::write(path, formatted)
                .map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        }
    }
    if check && unformatted > 0 {
        return Err(format!("{unformatted} file(s) not formatted"));
    }
    Ok(())
}

/// `rxh adapt`: prints the RXH configuration of a Caddyfile or nginx file.
fn adapt_file(path: &Path, adapter: Option<Adapter>) -> Result<(), String> {
    let text: String =
        fs::read_to_string(path).map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let adapter: Adapter = adapter.unwrap_or_else(|| adapt::detect(path, &text));
    let adapted = adapt::adapt(&text, adapter, &path.display().to_string())
        .map_err(|err| format!("{}: {err}", path.display()))?;
    print!("{}", adapted.toml);
    for warning in &adapted.warnings {
        eprintln!("rxh: not converted: {warning}");
    }
    if adapted.is_empty {
        return Err(String::from("nothing could be converted"));
    }
    // `{env.NAME}` placeholders from Caddy's `{$NAME}` are checked with the
    // current environment; without the variables the check is left to `rxh validate`.
    let expanded = placeholder::expand(&adapted.toml, |lookup| match lookup {
        Lookup::Env(name) => env::var(name).map_err(|err| err.to_string()),
        Lookup::File(path) => Err(format!("{path} is not read here")),
    });
    let Ok(expanded) = expanded else {
        return Ok(());
    };
    Config::from_toml(&expanded)
        .map(|_| ())
        .map_err(|err| format!("the converted configuration needs changes by hand: {err}"))
}

fn schema() -> Result<(), String> {
    let schema = Config::json_schema();
    let json: String = serde_json::to_string_pretty(&schema).map_err(|err| err.to_string())?;
    println!("{json}");
    Ok(())
}

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

    // Started by the daemon it replaces: already in the background, with
    // its output on the same log, and the admin address is busy on purpose.
    if rxh_server::is_upgrade() {
        process::take_over_pid_file(&cli.pid_file).map_err(|err| err.to_string())?;
        return run(cli);
    }

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
