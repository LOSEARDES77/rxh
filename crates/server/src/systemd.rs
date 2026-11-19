//! systemd integration: readiness notifications (`sd_notify`) and socket
//! activation (`LISTEN_FDS`). Outside of systemd, and on systems other than
//! Linux, both do nothing.
//!
//! Inherited sockets are matched to listeners by address: a listener whose
//! configured address equals the local address of an inherited socket of the
//! same type uses a duplicate of that socket instead of binding its own. Every
//! generation (reload) takes a new duplicate, so the socket outlives them all.

#[cfg(target_os = "linux")]
pub(crate) use linux::{notify, reloading, tcp_listener, udp_socket, unix_listener, warn_unused};

#[cfg(not(target_os = "linux"))]
pub(crate) use other::{notify, reloading, tcp_listener, udp_socket, warn_unused};

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) use other::unix_listener;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::OsStr;
    use std::fmt;
    use std::io;
    use std::net::{SocketAddr, TcpListener, UdpSocket};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::net::{UnixDatagram, UnixListener};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// First descriptor systemd passes, `SD_LISTEN_FDS_START`.
    const LISTEN_FDS_START: i32 = 3;

    #[derive(Debug, PartialEq)]
    enum Address {
        Stream(SocketAddr),
        Datagram(SocketAddr),
        Unix(PathBuf),
        /// FIFOs, sequential packet sockets, unnamed unix sockets...
        Unsupported,
    }

    impl fmt::Display for Address {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Stream(address) => write!(f, "TCP {address}"),
                Self::Datagram(address) => write!(f, "UDP {address}"),
                Self::Unix(path) => write!(f, "unix {}", path.display()),
                Self::Unsupported => f.write_str("of an unsupported type"),
            }
        }
    }

    struct Inherited {
        address: Address,
        fd: OwnedFd,
        used: AtomicBool,
    }

    static INHERITED: OnceLock<Vec<Inherited>> = OnceLock::new();

    fn inherited() -> &'static [Inherited] {
        INHERITED.get_or_init(inherit)
    }

    /// Takes the descriptors systemd passed to this process.
    fn inherit() -> Vec<Inherited> {
        let for_us = std::env::var("LISTEN_PID")
            .ok()
            .and_then(|pid| pid.parse::<u32>().ok())
            == Some(std::process::id());
        let count: i32 = std::env::var("LISTEN_FDS")
            .ok()
            .and_then(|count| count.parse().ok())
            .unwrap_or(0);
        if !for_us || count <= 0 {
            return Vec::new();
        }

        let mut sockets = Vec::new();
        for raw in LISTEN_FDS_START..LISTEN_FDS_START + count {
            // SAFETY: LISTEN_PID names this process, so systemd opened the
            // LISTEN_FDS descriptors from 3 on for it before exec, and this
            // runs once (`OnceLock`), so nothing else owns them.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            // Hooks and other children must not inherit them in turn.
            // SAFETY: `fd` is an open descriptor owned by this function.
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
            let address = address(&fd).unwrap_or(Address::Unsupported);
            sockets.push(Inherited {
                address,
                fd,
                used: AtomicBool::new(false),
            });
        }
        sockets
    }

    fn address(fd: &OwnedFd) -> io::Result<Address> {
        let mut kind: libc::c_int = 0;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `kind` and `length` are valid for writes and `length` is
        // the size of `kind`.
        let result = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut kind).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }

        let copy = fd.try_clone()?;
        match kind {
            libc::SOCK_STREAM => {
                let tcp = TcpListener::from(copy);
                // Not an IP socket: std refuses the unix address family.
                if let Ok(address) = tcp.local_addr() {
                    return Ok(Address::Stream(address));
                }
                let unix = UnixListener::from(OwnedFd::from(tcp));
                Ok(match unix.local_addr()?.as_pathname() {
                    Some(path) => Address::Unix(path.to_owned()),
                    None => Address::Unsupported,
                })
            }
            libc::SOCK_DGRAM => Ok(UdpSocket::from(copy)
                .local_addr()
                .map_or(Address::Unsupported, Address::Datagram)),
            _ => Ok(Address::Unsupported),
        }
    }

    /// Duplicate of the inherited socket at `wanted`, if there is one.
    fn take(wanted: &Address) -> io::Result<Option<OwnedFd>> {
        let Some(socket) = inherited().iter().find(|socket| socket.address == *wanted) else {
            return Ok(None);
        };
        socket.used.store(true, Ordering::Relaxed);
        socket.fd.try_clone().map(Some)
    }

    /// Inherited TCP listener bound to `address`.
    pub(crate) fn tcp_listener(address: SocketAddr) -> io::Result<Option<TcpListener>> {
        Ok(take(&Address::Stream(address))?.map(TcpListener::from))
    }

    /// Inherited UDP socket bound to `address`.
    pub(crate) fn udp_socket(address: SocketAddr) -> io::Result<Option<UdpSocket>> {
        Ok(take(&Address::Datagram(address))?.map(UdpSocket::from))
    }

    /// Inherited unix stream listener bound to `path`.
    pub(crate) fn unix_listener(path: &Path) -> io::Result<Option<UnixListener>> {
        Ok(take(&Address::Unix(path.to_owned()))?.map(UnixListener::from))
    }

    /// Reports the inherited sockets no listener took, which usually means
    /// a `ListenStream=` that differs from the `listen` of the configuration.
    pub(crate) fn warn_unused() {
        for socket in inherited() {
            if !socket.used.load(Ordering::Relaxed) {
                eprintln!(
                    "Main => systemd passed a socket {} that no listener uses",
                    socket.address
                );
            }
        }
    }

    /// Sends `state` to the service manager when `NOTIFY_SOCKET` is set.
    pub(crate) fn notify(state: &str) {
        let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
            return;
        };
        if let Err(err) = send(&path, state) {
            eprintln!("Main => Cannot notify systemd: {err}");
        }
    }

    fn send(path: &OsStr, state: &str) -> io::Result<()> {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::SocketAddr as UnixAddress;

        let address = match path.as_bytes().strip_prefix(b"@") {
            Some(name) => UnixAddress::from_abstract_name(name)?,
            None => UnixAddress::from_pathname(path)?,
        };
        UnixDatagram::unbound()?.send_to_addr(state.as_bytes(), &address)?;
        Ok(())
    }

    /// Tells systemd a reload started, as `Type=notify-reload` expects.
    pub(crate) fn reloading() {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `now` is valid for writes.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut now) };
        let microseconds = now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1_000;
        notify(&format!("RELOADING=1\nMONOTONIC_USEC={microseconds}"));
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn address_tells_socket_types_apart() {
            let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
            let tcp_address = tcp.local_addr().unwrap();
            assert_eq!(
                address(&OwnedFd::from(tcp)).unwrap(),
                Address::Stream(tcp_address)
            );

            let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
            let udp_address = udp.local_addr().unwrap();
            assert_eq!(
                address(&OwnedFd::from(udp)).unwrap(),
                Address::Datagram(udp_address)
            );

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("socket");
            let unix = UnixListener::bind(&path).unwrap();
            assert_eq!(address(&OwnedFd::from(unix)).unwrap(), Address::Unix(path));

            let (unnamed, _) = UnixDatagram::pair().unwrap();
            assert_eq!(
                address(&OwnedFd::from(unnamed)).unwrap(),
                Address::Unsupported
            );
        }

        #[test]
        fn notify_reaches_path_and_abstract_sockets() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("notify");
            let receiver = UnixDatagram::bind(&path).unwrap();
            send(path.as_os_str(), "READY=1").unwrap();
            let mut buffer = [0; 64];
            let length = receiver.recv(&mut buffer).unwrap();
            assert_eq!(&buffer[..length], b"READY=1");

            let name = format!("rxh-test-{}", std::process::id());
            let abstract_address = {
                use std::os::linux::net::SocketAddrExt;
                std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap()
            };
            let receiver = UnixDatagram::bind_addr(&abstract_address).unwrap();
            send(OsStr::new(&format!("@{name}")), "STOPPING=1").unwrap();
            let length = receiver.recv(&mut buffer).unwrap();
            assert_eq!(&buffer[..length], b"STOPPING=1");
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod other {
    use std::io;
    use std::net::{SocketAddr, TcpListener, UdpSocket};

    pub(crate) fn tcp_listener(_: SocketAddr) -> io::Result<Option<TcpListener>> {
        Ok(None)
    }

    pub(crate) fn udp_socket(_: SocketAddr) -> io::Result<Option<UdpSocket>> {
        Ok(None)
    }

    #[cfg(unix)]
    pub(crate) fn unix_listener(
        _: &std::path::Path,
    ) -> io::Result<Option<std::os::unix::net::UnixListener>> {
        Ok(None)
    }

    pub(crate) fn warn_unused() {}

    pub(crate) fn notify(_: &str) {}

    pub(crate) fn reloading() {}
}
