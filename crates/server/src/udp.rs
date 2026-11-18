//! Layer 4 UDP proxy. Each client address gets a session bound to one
//! backend, with its own upstream socket, until it stays idle for
//! `udp_idle_timeout`.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rxh_core::address::BackendAddress;
use rxh_core::pool::{RequestGuard, SelectContext, Upstream};
use tokio::net::UdpSocket;
use tokio::task::JoinSet;

use crate::registry::Group;

const MAX_DATAGRAM: usize = 65_535;

/// UDP listener settings.
#[derive(Debug, Clone)]
pub(crate) struct UdpSettings {
    pub(crate) group: Arc<Group>,
    pub(crate) idle_timeout: Duration,
    pub(crate) max_sessions: usize,
    pub(crate) log_name: Arc<str>,
}

#[derive(Debug, Clone)]
struct Session {
    backend: Arc<UdpSocket>,
    last_active: Arc<AtomicU64>,
}

type Sessions = Arc<Mutex<HashMap<SocketAddr, Session>>>;

/// Serves datagrams until the future is dropped. Dropping it also ends all
/// sessions.
pub(crate) async fn serve(socket: Arc<UdpSocket>, settings: UdpSettings) -> io::Result<()> {
    let sessions: Sessions = Arc::default();
    let epoch = Instant::now();
    let mut replies = JoinSet::new();
    let mut buffer = vec![0; MAX_DATAGRAM];

    loop {
        let (len, client) = socket.recv_from(&mut buffer).await?;
        while replies.try_join_next().is_some() {}

        let existing = sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&client)
            .cloned();
        let session = match existing {
            Some(session) => session,
            None => {
                if sessions
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len()
                    >= settings.max_sessions
                {
                    eprintln!(
                        "{} => max UDP sessions reached, dropping datagram from {client}",
                        settings.log_name
                    );
                    continue;
                }
                // ponytail: session setup (DNS + bind) runs inline in the
                // receive loop; move it to a task if setup latency matters.
                match open_session(&settings, client).await {
                    Ok((session, upstream)) => {
                        let guard = upstream.start_request();
                        replies.spawn(reply_loop(
                            Arc::clone(&socket),
                            session.clone(),
                            client,
                            epoch,
                            settings.idle_timeout,
                            guard,
                            Arc::clone(&sessions),
                        ));
                        sessions
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(client, session.clone());
                        session
                    }
                    Err(err) => {
                        eprintln!("{} => {client}: {err}", settings.log_name);
                        continue;
                    }
                }
            }
        };

        session
            .last_active
            .store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        if let Err(err) = session.backend.send(&buffer[..len]).await {
            eprintln!(
                "{} => {client}: send to backend failed: {err}",
                settings.log_name
            );
        }
    }
}

async fn open_session(
    settings: &UdpSettings,
    client: SocketAddr,
) -> io::Result<(Session, Arc<Upstream>)> {
    let selection = settings
        .group
        .pool
        .select(&SelectContext::l4(client.ip()))
        .ok_or_else(|| io::Error::other("no upstreams available"))?;
    let upstream = selection.upstream;

    let address = BackendAddress::parse(upstream.address())
        .ok()
        .and_then(|address| address.host_port())
        .ok_or_else(|| io::Error::other(format!("{} is not a udp address", upstream.address())))?;
    let target = tokio::net::lookup_host(address)
        .await?
        .next()
        .ok_or_else(|| io::Error::other(format!("{} did not resolve", upstream.address())))?;
    let bind: SocketAddr = if target.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0_u16; 8], 0))
    };

    let backend = UdpSocket::bind(bind).await?;
    backend.connect(target).await?;
    println!(
        "{client} -> {} UDP session to {}",
        settings.log_name,
        upstream.address()
    );

    let session = Session {
        backend: Arc::new(backend),
        last_active: Arc::default(),
    };
    Ok((session, upstream))
}

async fn reply_loop(
    listener: Arc<UdpSocket>,
    session: Session,
    client: SocketAddr,
    epoch: Instant,
    idle_timeout: Duration,
    guard: RequestGuard,
    sessions: Sessions,
) {
    let mut buffer = vec![0; MAX_DATAGRAM];

    loop {
        let last = Duration::from_millis(session.last_active.load(Ordering::Relaxed));
        let idle = epoch.elapsed().saturating_sub(last);
        let Some(remaining) = idle_timeout
            .checked_sub(idle)
            .filter(|left| !left.is_zero())
        else {
            break;
        };

        match tokio::time::timeout(remaining, session.backend.recv(&mut buffer)).await {
            Err(_) => continue,
            Ok(Ok(len)) => {
                session
                    .last_active
                    .store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                if let Err(err) = listener.send_to(&buffer[..len], client).await {
                    eprintln!("UDP reply to {client} failed: {err}");
                }
            }
            Ok(Err(err)) => {
                // Usually ICMP port unreachable surfacing as ECONNREFUSED.
                guard.upstream().record_failure();
                eprintln!(
                    "UDP backend {} for {client} failed: {err}",
                    guard.upstream().address()
                );
                break;
            }
        }
    }

    sessions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&client);
}
