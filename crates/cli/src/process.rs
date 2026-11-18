//! Process management: daemonizing and signalling through the PID file.
//! These are fallbacks for when the admin API is disabled.

use std::fs;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Signal sent to a daemon found through its PID file.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DaemonSignal {
    Terminate,
    Reload,
}

/// Forks into the background, writing the PID file and redirecting output to
/// `log_file`. Must run before the Tokio runtime starts.
#[cfg(unix)]
pub(crate) fn daemonize(pid_file: &Path, log_file: &Path) -> io::Result<()> {
    for path in [pid_file, log_file] {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
    }

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file)?;
    let errors = log.try_clone()?;

    daemonize::Daemonize::new()
        .working_directory(std::env::current_dir()?)
        .pid_file(pid_file)
        .stdout(log)
        .stderr(errors)
        .start()
        .map_err(io::Error::other)
}

/// Background mode needs `fork`, which only exists on Unix.
#[cfg(not(unix))]
pub(crate) fn daemonize(_pid_file: &Path, _log_file: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "`rxh start` is only supported on unix, use `rxh run` with a service manager",
    ))
}

/// Sends `signal` to the process in `pid_file`.
#[cfg(unix)]
pub(crate) fn signal(pid_file: &Path, signal: DaemonSignal) -> io::Result<i32> {
    let text = fs::read_to_string(pid_file)?;
    let pid: i32 = text.trim().parse().map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad PID in {}: {err}", pid_file.display()),
        )
    })?;
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad PID {pid} in {}", pid_file.display()),
        ));
    }

    let raw = match signal {
        DaemonSignal::Terminate => libc::SIGTERM,
        DaemonSignal::Reload => libc::SIGHUP,
    };

    // SAFETY: kill(2) takes plain integers and has no memory safety
    // requirements; `pid` is positive so it targets a single process.
    let status = unsafe { libc::kill(pid, raw) };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// Signals are only available on Unix.
#[cfg(not(unix))]
pub(crate) fn signal(_pid_file: &Path, _signal: DaemonSignal) -> io::Result<i32> {
    Err(io::Error::other(
        "signals are only supported on unix, enable the admin API",
    ))
}
