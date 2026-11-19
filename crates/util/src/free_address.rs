//! Picks the addresses the tests tell a server to listen on.

use std::mem;
use std::net::SocketAddr;

use tokio::net::TcpSocket;

/// A local TCP address nothing else takes before the server under test
/// listens on it, for tests that must know the address in advance.
///
/// The socket that picked the port stays bound, without listening, for the
/// rest of the process: the kernel then hands the port to no other `bind` to
/// port 0 and to no outgoing connection, while a listener that sets
/// `SO_REUSEADDR` (the default of std and tokio) can still bind it. Releasing
/// the port instead lets a test running in parallel take it first.
///
/// # Panics
///
/// Panics if no socket can be bound to localhost.
#[must_use]
pub fn free_address() -> SocketAddr {
    let socket = TcpSocket::new_v4().expect("socket");
    socket.set_reuseaddr(true).expect("SO_REUSEADDR");
    socket
        .bind(SocketAddr::from(([127, 0, 0, 1], 0)))
        .expect("bind");
    let address = socket.local_addr().expect("address");
    mem::forget(socket);
    address
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn stays_reserved_but_can_be_listened_on() {
        let address = free_address();
        // Port 0 never lands on a reserved port: the old helper could.
        for _ in 0..2000 {
            let other = TcpListener::bind("127.0.0.1:0").expect("bind");
            assert_ne!(other.local_addr().expect("address").port(), address.port());
        }
        let listener = TcpListener::bind(address).expect("the reserved address binds");
        assert_eq!(listener.local_addr().expect("address"), address);
    }
}
