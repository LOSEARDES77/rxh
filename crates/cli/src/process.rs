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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    use super::*;

    fn pid_file(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rxh.pid");
        fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn missing_pid_file_is_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = signal(&dir.path().join("none.pid"), DaemonSignal::Reload).expect_err("missing");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn unparsable_and_non_positive_pids_are_rejected() {
        // 0 and negative PIDs would signal a whole process group.
        for (contents, expected) in [
            ("abc", "bad PID in"),
            ("", "bad PID in"),
            ("0\n", "bad PID 0 in"),
            ("-1", "bad PID -1 in"),
        ] {
            let (_dir, path) = pid_file(contents);
            let err = signal(&path, DaemonSignal::Terminate).expect_err(contents);
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{contents:?}");
            assert!(err.to_string().contains(expected), "{contents:?}: {err}");
        }
    }

    #[test]
    fn stale_pid_reports_the_kill_error() {
        let mut child = Command::new("true").spawn().expect("spawn");
        let pid = child.id();
        child.wait().expect("wait");
        let (_dir, path) = pid_file(&pid.to_string());

        let err = signal(&path, DaemonSignal::Terminate).expect_err("process is gone");
        assert_eq!(err.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn signals_map_to_sigterm_and_sighup() {
        for (daemon_signal, expected) in [
            (DaemonSignal::Terminate, libc::SIGTERM),
            (DaemonSignal::Reload, libc::SIGHUP),
        ] {
            let mut child = Command::new("sleep").arg("30").spawn().expect("spawn");
            let (_dir, path) = pid_file(&format!("{}\n", child.id()));

            let pid = signal(&path, daemon_signal).expect("signal sent");
            assert_eq!(u32::try_from(pid).expect("positive"), child.id());
            let status = child.wait().expect("wait");
            assert_eq!(status.signal(), Some(expected), "{daemon_signal:?}");
        }
    }
}
