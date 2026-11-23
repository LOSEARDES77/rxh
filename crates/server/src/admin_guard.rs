//! Keeps the data plane away from the admin API of this process.
//!
//! Without `[admin] token`, a request that reaches the admin API over a
//! connection from this machine is an administrator. A backend address that
//! leads to the admin API would make the proxy open that connection for
//! anybody on the network, so [`Dialer`](crate::stream::Dialer) asks an
//! [`AdminGuard`] before it connects.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rxh_core::config::{Admin, AdminListen};

/// The admin listeners upstream connections must not reach, if there are
/// any.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AdminGuard {
    listen: Option<AdminListen>,
    /// `[admin] socket`.
    socket: Option<PathBuf>,
}

impl AdminGuard {
    /// Guard for the `[admin]` table in force: its listeners when the API is
    /// enabled without a token, nothing otherwise.
    pub(crate) fn new(admin: &Admin) -> Self {
        let guarded = admin.enabled && admin.token.is_none();
        Self {
            listen: guarded.then(|| admin.listen.clone()),
            socket: admin.socket.clone().filter(|_| guarded),
        }
    }

    /// Address of the guarded TCP listener if it is on `port`: only
    /// connections to that port need [`targets_admin`].
    pub(crate) fn tcp_on(&self, port: u16) -> Option<SocketAddr> {
        match &self.listen {
            Some(AdminListen::Tcp(admin)) if admin.port() == port => Some(*admin),
            Some(_) | None => None,
        }
    }

    /// True if `path` is a guarded unix socket, under this or another name
    /// (symbolic or hard link).
    pub(crate) fn is_socket(&self, path: &Path) -> bool {
        let listen = match &self.listen {
            Some(AdminListen::Unix(admin)) => Some(admin),
            Some(AdminListen::Tcp(_)) | None => None,
        };
        let sockets = [listen, self.socket.as_ref()];
        sockets
            .into_iter()
            .flatten()
            .any(|admin| path == admin || same_file(path, admin))
    }
}

#[cfg(unix)]
fn same_file(path: &Path, other: &Path) -> bool {
    use std::fs;
    use std::os::unix::fs::MetadataExt;

    match (fs::metadata(path), fs::metadata(other)) {
        (Ok(path), Ok(other)) => path.dev() == other.dev() && path.ino() == other.ino(),
        // A path that cannot be read cannot be connected to either.
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_file(_path: &Path, _other: &Path) -> bool {
    false
}

/// True if a TCP connection to `destination` arrives at the admin API
/// listening on `admin` as a connection from this machine.
///
/// `source` is the local address of the socket once it is connected, `None`
/// before. The admin API calls a connection local when its peer is a
/// loopback address or the address it was accepted on, which is this
/// socket's source being loopback or equal to its destination: the kernel
/// answers for one of its own addresses with that same address. So the
/// source says whether `destination` is this machine without a list of its
/// addresses, which would go stale. Loopback destinations, and the
/// unspecified address (which the kernel routes to loopback), are refused
/// before connecting, on any loopback address: `127.0.0.2` does not reach a
/// listener on `127.0.0.1`, but nothing legitimate is lost by refusing it.
/// IPv4-mapped IPv6 addresses count as the IPv4 address they hold.
pub(crate) fn targets_admin(
    destination: SocketAddr,
    source: Option<IpAddr>,
    admin: SocketAddr,
) -> bool {
    if destination.port() != admin.port() {
        return false;
    }
    let to = destination.ip().to_canonical();
    let listen = admin.ip().to_canonical();
    let loopback = to.is_loopback() || to.is_unspecified();
    let own_address = source.is_some_and(|source| {
        let source = source.to_canonical();
        source.is_loopback() || source == to
    });
    to == listen
        || (loopback && (listen.is_loopback() || listen.is_unspecified()))
        || (own_address && listen.is_unspecified())
}

/// The error of a connection that was not opened because it leads to the
/// admin API. Logs why, once a minute at most.
pub(crate) fn refused() -> io::Error {
    // Seconds since the epoch of the last line, 0 before the first.
    static LOGGED: AtomicU64 = AtomicU64::new(0);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |elapsed| elapsed.as_secs());
    let last = LOGGED.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= 60
        && LOGGED
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        eprintln!(
            "Admin => refused to proxy to the admin API of this process; set [admin] token to \
             allow it"
        );
    }
    io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "the admin API of this process is not a backend without [admin] token",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    fn targets(destination: &str, source: Option<&str>, admin: &str) -> bool {
        let source = source.map(|source| source.parse().unwrap());
        targets_admin(address(destination), source, address(admin))
    }

    #[test]
    fn the_admin_address_and_port_is_refused() {
        assert!(targets("127.0.0.1:2019", None, "127.0.0.1:2019"));
        assert!(targets("192.168.1.5:2019", None, "192.168.1.5:2019"));
        assert!(targets("[::1]:2019", None, "[::1]:2019"));
    }

    #[test]
    fn another_port_is_allowed() {
        assert!(!targets("127.0.0.1:8080", None, "127.0.0.1:2019"));
        assert!(!targets(
            "127.0.0.1:8080",
            Some("127.0.0.1"),
            "0.0.0.0:2019"
        ));
    }

    #[test]
    fn loopback_is_refused_for_a_loopback_or_unspecified_admin() {
        assert!(targets("127.0.0.1:2019", None, "0.0.0.0:2019"));
        assert!(targets("127.0.0.1:2019", None, "[::]:2019"));
        assert!(targets("[::1]:2019", None, "[::]:2019"));
        // Does not reach a listener on 127.0.0.1; refused all the same.
        assert!(targets("127.0.0.2:2019", None, "127.0.0.1:2019"));
        assert!(targets("[::1]:2019", None, "127.0.0.1:2019"));
    }

    #[test]
    fn ipv4_mapped_addresses_count_as_ipv4() {
        assert!(targets("[::ffff:127.0.0.1]:2019", None, "127.0.0.1:2019"));
        assert!(targets("127.0.0.1:2019", None, "[::ffff:127.0.0.1]:2019"));
        assert!(targets(
            "[::ffff:192.168.1.5]:2019",
            Some("192.168.1.5"),
            "0.0.0.0:2019"
        ));
    }

    #[test]
    fn the_unspecified_destination_is_loopback() {
        assert!(targets("0.0.0.0:2019", None, "127.0.0.1:2019"));
        assert!(targets("[::]:2019", None, "0.0.0.0:2019"));
    }

    #[test]
    fn an_address_of_this_machine_is_refused_for_an_unspecified_admin() {
        // Not known before connecting.
        assert!(!targets("192.168.1.5:2019", None, "0.0.0.0:2019"));
        assert!(targets(
            "192.168.1.5:2019",
            Some("192.168.1.5"),
            "0.0.0.0:2019"
        ));
        assert!(targets("192.168.1.5:2019", Some("127.0.0.1"), "[::]:2019"));
    }

    #[test]
    fn a_remote_address_with_the_admin_port_is_allowed() {
        assert!(!targets("192.168.1.9:2019", None, "127.0.0.1:2019"));
        assert!(!targets(
            "192.168.1.9:2019",
            Some("192.168.1.5"),
            "127.0.0.1:2019"
        ));
        assert!(!targets(
            "192.168.1.9:2019",
            Some("192.168.1.5"),
            "0.0.0.0:2019"
        ));
        // An admin on one address of the machine is not reached on another.
        assert!(!targets(
            "192.168.1.5:2019",
            Some("192.168.1.5"),
            "10.0.0.5:2019"
        ));
    }

    #[test]
    fn a_guard_exists_only_for_an_enabled_admin_without_a_token() {
        let open = Admin {
            listen: AdminListen::Tcp(address("127.0.0.1:2019")),
            ..Admin::default()
        };
        assert_eq!(
            AdminGuard::new(&open).tcp_on(2019),
            Some(address("127.0.0.1:2019"))
        );
        assert_eq!(AdminGuard::new(&open).tcp_on(80), None);

        let with_token = Admin {
            token: Some("0123456789abcdef".to_owned()),
            ..open.clone()
        };
        assert_eq!(AdminGuard::new(&with_token), AdminGuard::default());
        let disabled = Admin {
            enabled: false,
            ..open
        };
        assert_eq!(AdminGuard::new(&disabled), AdminGuard::default());
    }

    #[cfg(unix)]
    #[test]
    fn the_admin_socket_is_recognised_under_another_name() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("admin.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let link = directory.path().join("link.sock");
        std::os::unix::fs::symlink(&socket, &link).unwrap();
        let other = directory.path().join("other.sock");
        let _other = std::os::unix::net::UnixListener::bind(&other).unwrap();

        let guard = AdminGuard::new(&Admin {
            listen: AdminListen::Unix(socket.clone()),
            ..Admin::default()
        });
        assert!(guard.is_socket(&socket));
        assert!(guard.is_socket(&link));
        assert!(!guard.is_socket(&other));
        assert!(!AdminGuard::default().is_socket(&socket));

        // `[admin] socket` is guarded beside a TCP listen, unless a token
        // is the check.
        let beside = Admin {
            socket: Some(socket.clone()),
            ..Admin::default()
        };
        assert!(AdminGuard::new(&beside).is_socket(&socket));
        assert!(AdminGuard::new(&beside).is_socket(&link));
        assert!(!AdminGuard::new(&beside).is_socket(&other));
        let with_token = Admin {
            token: Some("0123456789abcdef".to_owned()),
            ..beside
        };
        assert!(!AdminGuard::new(&with_token).is_socket(&socket));
    }
}
