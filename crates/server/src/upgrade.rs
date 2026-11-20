//! Hot binary upgrade: a new process takes over the listening sockets.
//!
//! On `SIGUSR2` or `POST /upgrade` the running daemon starts its own command
//! line again, which runs the binary now on disk, and leaves it duplicates
//! of every listening socket and the write end of a pipe:
//!
//! ```text
//!   old process                         new process
//!       |  spawn, RXH_UPGRADE_FDS=pipe,socket,socket...
//!       |---------------------------------->|
//!       |                                   |  loads the configuration,
//!       |                                   |  listeners take the sockets
//!       |<------------- one byte -----------|  ready
//!       |  stops accepting, drains, exits   |  serves
//! ```
//!
//! Both processes hold the same sockets while the new one starts, so no
//! connection is refused and the backlog is never dropped. If the new process
//! exits before it is ready the pipe closes without the byte and the old one
//! keeps serving. Linux only.

#[cfg(target_os = "linux")]
pub use linux::is_upgrade;
#[cfg(target_os = "linux")]
pub(crate) use linux::{Lease, handed_over, lease, ready, spawn, take};

#[cfg(not(target_os = "linux"))]
pub use other::is_upgrade;
#[cfg(not(target_os = "linux"))]
pub(crate) use other::{Lease, handed_over, lease, ready, spawn};

#[cfg(target_os = "linux")]
mod linux {
    use std::env;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use crate::systemd::{self, Address};

    /// Descriptor numbers left open for the new process: the readiness pipe,
    /// then the sockets.
    const ENV: &str = "RXH_UPGRADE_FDS";

    /// Duplicates of the sockets this process listens on.
    static LIVE: Mutex<Vec<(u64, OwnedFd)>> = Mutex::new(Vec::new());

    static HANDED_OVER: AtomicBool = AtomicBool::new(false);

    /// What the previous process left to this one.
    struct Received {
        ready: Option<OwnedFd>,
        sockets: Vec<(Address, OwnedFd)>,
    }

    static RECEIVED: LazyLock<Mutex<Received>> = LazyLock::new(|| Mutex::new(receive()));

    fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .expect("nothing panics while holding the upgrade locks")
    }

    /// Keeps a listening socket on the list of those handed to the next
    /// process. Drop it together with the socket: the duplicate it stands
    /// for keeps the port open.
    pub(crate) struct Lease(u64);

    impl Drop for Lease {
        fn drop(&mut self) {
            locked(&LIVE).retain(|(id, _)| *id != self.0);
        }
    }

    /// Registers `socket` for the next upgrade.
    pub(crate) fn lease(socket: &impl AsFd) -> io::Result<Lease> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let fd = socket.as_fd().try_clone_to_owned()?;
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        locked(&LIVE).push((id, fd));
        Ok(Lease(id))
    }

    /// Whether a running daemon started this process to hand it its
    /// listening sockets (hot binary upgrade), so the addresses of the
    /// configuration are busy on purpose.
    #[must_use]
    pub fn is_upgrade() -> bool {
        env::var_os(ENV).is_some()
    }

    /// Whether the new process took over, so what it now owns (the admin
    /// socket file) must be left alone on the way out.
    pub(crate) fn handed_over() -> bool {
        HANDED_OVER.load(Ordering::Relaxed)
    }

    fn receive() -> Received {
        let numbers = env::var(ENV).unwrap_or_default();
        let mut fds = numbers.split(',').map(|number| {
            let raw: RawFd = number.parse().ok().filter(|&raw| raw > 2)?;
            // SAFETY: F_GETFD only reads the flags of a descriptor number.
            if unsafe { libc::fcntl(raw, libc::F_GETFD) } < 0 {
                return None;
            }
            // SAFETY: the process that started this one left `raw` open for
            // it and named it in the variable; this runs once (`LazyLock`),
            // so nothing else owns it.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // Hooks and other children must not inherit it in turn.
            // SAFETY: `fd` is an open descriptor owned by this function.
            unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) };
            Some(fd)
        });
        let ready = fds.next().flatten();
        let sockets = fds
            .flatten()
            .filter_map(|fd| Some((systemd::address(&fd).ok()?, fd)))
            .collect();
        Received { ready, sockets }
    }

    /// The handed over socket bound to `wanted`. Each is taken once: the
    /// listener that gets it owns it, like one it bound.
    pub(crate) fn take(wanted: &Address) -> Option<OwnedFd> {
        let sockets = &mut locked(&RECEIVED).sockets;
        let position = sockets
            .iter()
            .position(|(address, _)| is_bound_to(address, wanted))?;
        Some(sockets.swap_remove(position).1)
    }

    /// Whether a socket at `address` listens on `wanted`. The admin unix
    /// socket is bound as `PATH.PID` and renamed to `PATH`, and keeps
    /// reporting the first name.
    fn is_bound_to(address: &Address, wanted: &Address) -> bool {
        if address == wanted {
            return true;
        }
        let (Address::Unix(bound), Address::Unix(wanted)) = (address, wanted) else {
            return false;
        };
        bound.file_stem() == wanted.file_name()
            && bound.parent() == wanted.parent()
            && bound
                .extension()
                .and_then(|pid| pid.to_str())
                .is_some_and(|pid| pid.parse::<u32>().is_ok())
    }

    /// Tells the previous process, if there is one, that this one serves,
    /// and closes the sockets no listener took: left open, they would queue
    /// connections nobody accepts.
    pub(crate) fn ready() {
        let mut received = locked(&RECEIVED);
        for (address, _) in received.sockets.drain(..) {
            eprintln!("Main => Closing the handed over socket {address}, no listener uses it");
        }
        if let Some(pipe) = received.ready.take()
            && let Err(err) = std::fs::File::from(pipe).write_all(b"1")
        {
            eprintln!("Main => Cannot tell the previous process to stop: {err}");
        }
    }

    /// Starts the command line of this process again, with its listening
    /// sockets. Returns the process ID and a blocking wait for the outcome,
    /// which must run: it reaps a process that fails.
    pub(crate) fn spawn() -> io::Result<(u32, impl FnOnce() -> Result<(), String> + Send)> {
        let mut arguments = env::args_os();
        let program = arguments
            .next()
            .ok_or_else(|| io::Error::other("no program name to start again"))?;
        let (mut reader, writer) = io::pipe()?;

        // One socket per address: with `runtime = "per_core"` every runtime
        // has its own, and the new process binds the others again.
        let mut passed: Vec<(Address, OwnedFd)> = Vec::new();
        for (_, fd) in locked(&LIVE).iter() {
            let address = systemd::address(fd)?;
            if !passed.iter().any(|(seen, _)| *seen == address) {
                passed.push((address, fd.try_clone()?));
            }
        }
        let raw: Vec<RawFd> = std::iter::once(writer.as_raw_fd())
            .chain(passed.iter().map(|(_, fd)| fd.as_raw_fd()))
            .collect();
        let numbers: Vec<String> = raw.iter().map(RawFd::to_string).collect();

        let mut command = Command::new(program);
        command.args(arguments).env(ENV, numbers.join(","));
        // SAFETY: fcntl(2) is async-signal-safe and `raw` was allocated
        // before the fork. Clearing close-on-exec here, in the child, keeps
        // the descriptors out of every other process this one starts.
        unsafe {
            command.pre_exec(move || {
                for &fd in &raw {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        // The read below ends when the new process closes its write end, so
        // this one must not keep a copy.
        drop(writer);
        drop(passed);

        let pid = child.id();
        Ok((pid, move || {
            let mut byte = [0; 1];
            match reader.read(&mut byte) {
                Ok(1) => {
                    HANDED_OVER.store(true, Ordering::Relaxed);
                    Ok(())
                }
                Ok(_) => Err(match child.wait() {
                    Ok(status) => format!("the new process stopped before it was ready ({status})"),
                    Err(err) => format!("the new process stopped before it was ready: {err}"),
                }),
                Err(err) => Err(format!("cannot wait for the new process: {err}")),
            }
        }))
    }

    #[cfg(test)]
    mod tests {
        use std::net::TcpListener;

        use super::*;

        #[test]
        fn leases_list_a_socket_until_they_are_dropped() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = Address::Stream(listener.local_addr().unwrap());
            let listed = || {
                locked(&LIVE)
                    .iter()
                    .filter(|(_, fd)| systemd::address(fd).unwrap() == address)
                    .count()
            };

            let first = lease(&listener).unwrap();
            let second = lease(&listener).unwrap();
            assert_eq!(listed(), 2);
            drop(first);
            assert_eq!(listed(), 1);
            drop(second);
            assert_eq!(listed(), 0, "a dropped lease must not keep the port open");
        }

        #[test]
        fn renamed_unix_sockets_match_their_path() {
            let unix = |path: &str| Address::Unix(path.into());
            let wanted = unix("/run/rxh/admin.sock");
            assert!(is_bound_to(&unix("/run/rxh/admin.sock"), &wanted));
            assert!(is_bound_to(&unix("/run/rxh/admin.sock.4242"), &wanted));
            assert!(!is_bound_to(&unix("/run/rxh/admin.sock.old"), &wanted));
            assert!(!is_bound_to(&unix("/run/other/admin.sock.4242"), &wanted));
            assert!(!is_bound_to(&unix("/run/rxh/other.sock.4242"), &wanted));
            let tcp = Address::Stream("127.0.0.1:80".parse().unwrap());
            assert!(!is_bound_to(&tcp, &wanted));
        }

        #[test]
        fn nothing_is_received_without_a_previous_process() {
            assert!(!is_upgrade());
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = Address::Stream(listener.local_addr().unwrap());
            assert!(take(&address).is_none());
            ready();
            assert!(!handed_over());
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::io;

    pub(crate) struct Lease;

    pub(crate) fn lease<T>(_: &T) -> io::Result<Lease> {
        Ok(Lease)
    }

    /// Hot binary upgrades are Linux only.
    #[must_use]
    pub fn is_upgrade() -> bool {
        false
    }

    pub(crate) fn handed_over() -> bool {
        false
    }

    pub(crate) fn ready() {}

    pub(crate) fn spawn() -> io::Result<(u32, fn() -> Result<(), String>)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hot binary upgrades are only supported on Linux",
        ))
    }
}
