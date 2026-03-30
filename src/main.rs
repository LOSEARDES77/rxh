use std::{
    env,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use clap::Parser;

fn main() -> Result<(), rxh::Error> {
    let cli = Cli::parse();

    if cli.kill_daemon {
        return signal_daemon(&cli.pid_file, DaemonSignal::Terminate);
    }

    if cli.reload_config {
        return signal_daemon(&cli.pid_file, DaemonSignal::Reload);
    }

    if cli.daemon {
        if let Some(parent) = cli.pid_file.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(rxh::Error::from)?;
        }

        let stdout_path = cli.pid_file.with_extension("log");
        let stdout = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&stdout_path)
            .unwrap_or_else(|err| {
                panic!("Failed to open log file {}: {err}", stdout_path.display())
            });
        let stderr = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(cli.pid_file.with_extension("err.log"))
            .unwrap_or_else(|err| {
                panic!(
                    "Failed to open error log file {}: {err}",
                    cli.pid_file.with_extension("err.log").display()
                )
            });
        let cwd = env::current_dir().map_err(rxh::Error::from)?;
        println!("Daemon PID will be written to {}", cli.pid_file.display());

        daemonize::Daemonize::new()
            .working_directory(cwd)
            .pid_file(&cli.pid_file)
            .stdout(stdout)
            .stderr(stderr)
            .start()
            .map_err(std::io::Error::other)?;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(rxh::Error::from)?;

    runtime.block_on(async_main(&cli.config))
}

#[derive(Debug, Parser)]
#[command(version = rxh::VERSION, about = "RXH reverse proxy and static files server")]
struct Cli {
    #[arg(
        short,
        long,
        help = "Run RXH as a daemon in the background",
        conflicts_with_all = ["kill_daemon", "reload_config"]
    )]
    daemon: bool,

    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "rxh.toml",
        help = "Path to the configuration file"
    )]
    config: PathBuf,

    #[arg(
        long,
        value_name = "FILE",
        default_value = "/tmp/rxh/rxh.pid",
        help = "PID file used to control a daemonized process"
    )]
    pid_file: PathBuf,

    #[arg(
        long,
        help = "Send SIGTERM to the daemon from --pid-file",
        conflicts_with = "reload_config"
    )]
    kill_daemon: bool,

    #[arg(
        long,
        help = "Send SIGHUP to the daemon from --pid-file",
        conflicts_with = "kill_daemon"
    )]
    reload_config: bool,
}

#[derive(Clone, Copy)]
enum DaemonSignal {
    Terminate,
    Reload,
}

impl DaemonSignal {
    fn as_raw(self) -> i32 {
        match self {
            DaemonSignal::Terminate => libc::SIGTERM,
            DaemonSignal::Reload => libc::SIGHUP,
        }
    }

    fn name(self) -> &'static str {
        match self {
            DaemonSignal::Terminate => "SIGTERM",
            DaemonSignal::Reload => "SIGHUP",
        }
    }
}

fn signal_daemon(pid_file: &Path, signal: DaemonSignal) -> Result<(), rxh::Error> {
    #[cfg(not(unix))]
    {
        let _ = pid_file;
        let _ = signal;
        return Err(
            std::io::Error::other("daemon control options are only supported on unix").into(),
        );
    }

    #[cfg(unix)]
    {
        let pid = std::fs::read_to_string(pid_file).map_err(rxh::Error::from)?;
        let pid = pid.trim().parse::<i32>().map_err(|err| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid PID in {}: {err}", pid_file.display()),
            )
        })?;

        if pid <= 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid non-positive PID in {}", pid_file.display()),
            )
            .into());
        }

        // libc::kill returns 0 on success and -1 on failure.
        let status = unsafe { libc::kill(pid, signal.as_raw()) };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }

        println!(
            "Sent {} to process {} (pid file: {})",
            signal.name(),
            pid,
            pid_file.display()
        );
        Ok(())
    }
}

async fn async_main(config_path: &Path) -> Result<(), rxh::Error> {
    loop {
        let config = toml::from_str(&tokio::fs::read_to_string(config_path).await?)?;
        let reason = run_once(config).await?;

        if reason == ShutdownReason::Exit {
            return Ok(());
        }

        println!("Main => Reloading {}", config_path.display());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShutdownReason {
    Exit,
    Reload,
}

async fn run_once(config: rxh::config::Config) -> Result<ShutdownReason, rxh::Error> {
    let reason = Arc::new(AtomicU8::new(0));
    let owned_reason = reason.clone();

    let (tx, rx) = tokio::sync::oneshot::channel();

    let signal_task = tokio::task::spawn(async move {
        let reason = wait_for_signal().await;
        owned_reason.store(reason as u8, Ordering::SeqCst);
        let _ = tx.send(());
    });

    let result = rxh::Master::init(config)?
        .shutdown_on(async {
            let _ = rx.await;
        })
        .run()
        .await;

    signal_task.abort();
    result?;

    Ok(match reason.load(Ordering::SeqCst) {
        1 => ShutdownReason::Reload,
        _ => ShutdownReason::Exit,
    })
}

async fn wait_for_signal() -> ShutdownReason {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut term = signal(SignalKind::terminate()).expect("failed to subscribe to SIGTERM");
        let mut hup = signal(SignalKind::hangup()).expect("failed to subscribe to SIGHUP");

        tokio::select! {
            _ = tokio::signal::ctrl_c() => ShutdownReason::Exit,
            _ = term.recv() => ShutdownReason::Exit,
            _ = hup.recv() => ShutdownReason::Reload,
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        ShutdownReason::Exit
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Cli;

    #[test]
    fn daemon_flag_parsing() {
        let args = ["rxh", "--daemon"];
        let cli = Cli::parse_from(args);
        assert!(cli.daemon);
    }

    #[test]
    fn control_flag_parsing() {
        let args = ["rxh", "--kill-daemon", "--pid-file", "custom.pid"];
        let cli = Cli::parse_from(args);
        assert!(cli.kill_daemon);
        assert_eq!(cli.pid_file, std::path::PathBuf::from("custom.pid"));
    }

    #[test]
    fn control_flags_conflict() {
        let args = ["rxh", "--kill-daemon", "--reload-config"];
        assert!(Cli::try_parse_from(args).is_err());
    }
}
