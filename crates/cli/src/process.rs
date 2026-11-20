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
    Upgrade,
}

/// Forks into the background, writing the PID file and redirecting output to
/// `log_file`. Must run before the Tokio runtime starts, while the process
/// has a single thread.
///
/// The PID file is locked here, in the foreground, so a second daemon with
/// the same PID file fails with a visible error; the daemon keeps the lock
/// until it exits. The PID is written before this process exits.
#[cfg(unix)]
pub(crate) fn daemonize(pid_file: &Path, log_file: &Path) -> io::Result<()> {
    use std::fs::TryLockError;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::process;

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
    let mut pid = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(pid_file)?;
    pid.try_lock().map_err(|err| match err {
        TryLockError::WouldBlock => io::Error::other(format!(
            "{} is locked, is rxh already running?",
            pid_file.display()
        )),
        TryLockError::Error(err) => err,
    })?;
    let null = OpenOptions::new().read(true).open("/dev/null")?;

    if let Some(child) = fork()? {
        let mut status = 0;
        // SAFETY: `status` is a valid out pointer and `child` is our child.
        if unsafe { libc::waitpid(child, &mut status, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            return Err(io::Error::other("the background process failed to start"));
        }
        process::exit(0);
    }

    // First child: leave the terminal's session, then fork again so the
    // daemon is not a session leader and never reacquires a terminal.
    let daemon = (|| {
        // SAFETY: setsid(2) takes no arguments; this child is not a process
        // group leader, so it cannot fail with EPERM.
        if unsafe { libc::setsid() } < 0 {
            return Err(io::Error::last_os_error());
        }
        fork()
    })();
    match daemon {
        Ok(Some(daemon)) => {
            let written = pid.set_len(0).and_then(|()| writeln!(pid, "{daemon}"));
            if let Err(err) = written {
                eprintln!("cannot write {}: {err}", pid_file.display());
                process::exit(1);
            }
            process::exit(0);
        }
        Ok(None) => {}
        Err(err) => {
            eprintln!("cannot detach: {err}");
            process::exit(1);
        }
    }

    // SAFETY: umask(2) only sets the file mode creation mask.
    unsafe { libc::umask(0o027) };
    for (from, to) in [
        (&null, libc::STDIN_FILENO),
        (&log, libc::STDOUT_FILENO),
        (&log, libc::STDERR_FILENO),
    ] {
        // SAFETY: both descriptors are open; dup2(2) atomically replaces `to`.
        if unsafe { libc::dup2(from.as_raw_fd(), to) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    // The lock lasts as long as the descriptor stays open: keep it for the
    // daemon's lifetime. It is close-on-exec, like every std file.
    std::mem::forget(pid);
    Ok(())
}

/// Makes the PID file name this process once the daemon it replaces (hot
/// binary upgrade) exits. That daemon holds the lock until then, so the wait
/// runs on a thread; from then on this process holds it.
#[cfg(unix)]
pub(crate) fn take_over_pid_file(pid_file: &Path) -> io::Result<()> {
    use std::io::Write;
    use std::{process, thread};

    let mut pid = OpenOptions::new().write(true).open(pid_file)?;
    thread::spawn(move || {
        let written = pid
            .lock()
            .and_then(|()| pid.set_len(0))
            .and_then(|()| writeln!(pid, "{}", process::id()));
        if let Err(err) = written {
            eprintln!("Main => Cannot take over the PID file: {err}");
        }
        // Locked for as long as the descriptor stays open.
        std::mem::forget(pid);
    });
    Ok(())
}

/// Hot binary upgrades are Linux only.
#[cfg(not(unix))]
pub(crate) fn take_over_pid_file(_pid_file: &Path) -> io::Result<()> {
    Ok(())
}

/// Forks the process: `Some(child)` in the parent, `None` in the child.
#[cfg(unix)]
fn fork() -> io::Result<Option<libc::pid_t>> {
    // SAFETY: callers run before any other thread exists, so the child gets
    // a consistent copy of the whole process.
    match unsafe { libc::fork() } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(None),
        child => Ok(Some(child)),
    }
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
        DaemonSignal::Upgrade => libc::SIGUSR2,
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
    use std::os::unix::process::{CommandExt, ExitStatusExt};
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
            (DaemonSignal::Upgrade, libc::SIGUSR2),
        ] {
            let mut command = Command::new("sleep");
            command.arg("30");
            // The test runner may ignore or block SIGHUP (`nohup`), and `exec`
            // keeps both, so the child would survive the signal.
            // SAFETY: only async-signal-safe calls between fork and exec,
            // operating on a stack-local signal set.
            unsafe {
                command.pre_exec(move || {
                    libc::signal(expected, libc::SIG_DFL);
                    let mut set: libc::sigset_t = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, expected);
                    libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
                    Ok(())
                });
            }
            let mut child = command.spawn().expect("spawn");
            let (_dir, path) = pid_file(&format!("{}\n", child.id()));

            let pid = signal(&path, daemon_signal).expect("signal sent");
            assert_eq!(u32::try_from(pid).expect("positive"), child.id());
            let status = child.wait().expect("wait");
            assert_eq!(status.signal(), Some(expected), "{daemon_signal:?}");
        }
    }
}
