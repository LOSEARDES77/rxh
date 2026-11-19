//! `mirror` entries of a proxy route: copies of requests sent in the
//! background to other pools, their responses dropped (shadow traffic).

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use hyper::Request;
use rxh_core::config;
use rxh_core::pool::SelectContext;

use crate::http::body::Body;
use crate::http::proxy;
use crate::http::service::Site;
use crate::http::transport::SendError;
use crate::registry::Group;
use crate::upstream::{self, DialError};

/// Copies in flight per mirror past which requests go without one. Shadow
/// traffic is best effort: a slow mirror must not pile up tasks and bodies.
const MAX_IN_FLIGHT: usize = 1024;

/// Time a copy may take, response body included.
const TIMEOUT: Duration = Duration::from_secs(30);

/// One compiled `mirror` entry.
#[derive(Debug)]
pub(crate) struct Mirror {
    group: Arc<Group>,
    percent: u64,
    max_body_size: usize,
    /// Requests seen, to copy exactly `percent` of every 100.
    seen: AtomicU64,
    in_flight: Arc<AtomicUsize>,
}

/// What a copy needs to know about the client of the original request.
pub(crate) struct Client {
    pub(crate) peer_ip: IpAddr,
    pub(crate) client_ip: IpAddr,
    pub(crate) secure: bool,
    /// PROXY protocol header of the `forward`, sent to the mirror too.
    pub(crate) preface: Vec<u8>,
    /// The route sets `Host` itself, so HTTPS backends do not get their own.
    pub(crate) sets_host: bool,
}

impl Mirror {
    pub(crate) fn new(group: Arc<Group>, config: &config::Mirror) -> Self {
        Self {
            group,
            percent: u64::from(config.percent),
            max_body_size: config.max_body_size,
            seen: AtomicU64::new(0),
            in_flight: Arc::default(),
        }
    }

    pub(crate) fn group(&self) -> &Group {
        &self.group
    }

    /// Whether the current request gets a copy: `percent` of every 100
    /// requests, spread evenly.
    pub(crate) fn takes_turn(&self) -> bool {
        let seen = self.seen.fetch_add(1, Ordering::Relaxed) % 100;
        (seen + 1) * self.percent / 100 > seen * self.percent / 100
    }

    /// Whether a body of `len` bytes (`None`: unknown) is copied.
    pub(crate) fn fits(&self, len: Option<u64>) -> bool {
        len.is_some_and(|len| len <= self.max_body_size as u64)
    }

    /// Sends `request` to a member of the pool in the background and drops
    /// the response. Failures are logged and fed to passive health checks.
    pub(crate) fn spawn(&self, request: Request<Body>, client: Client, site: Arc<Site>) {
        if self.in_flight.fetch_add(1, Ordering::Relaxed) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::Relaxed);
            return;
        }
        let group = Arc::clone(&self.group);
        let in_flight = Arc::clone(&self.in_flight);
        tokio::spawn(async move {
            let name = group.pool.name();
            match tokio::time::timeout(TIMEOUT, copy(&group, request, &client)).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) => eprintln!("{} => mirror @{name}: {err}", site.log_name),
                Err(_) => eprintln!(
                    "{} => mirror @{name}: no complete response within {TIMEOUT:?}",
                    site.log_name
                ),
            }
            in_flight.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn copy(group: &Group, mut request: Request<Body>, client: &Client) -> Result<(), String> {
    let preface = client.preface.as_slice();
    let context = SelectContext {
        peer_ip: client.peer_ip,
        client_ip: client.client_ip,
        uri: Some(request.uri()),
        headers: Some(request.headers()),
        secure: client.secure,
    };
    let mut retry_permit = None;
    let dialed = upstream::connect_with_retries(
        &group.pool,
        &context,
        &mut retry_permit,
        |upstream| async move { group.transport.connect(upstream.address(), preface).await },
    )
    .await;
    let (selection, _guard, connection) = match dialed {
        Ok(dialed) => dialed,
        Err(DialError::NoUpstream) => return Err(String::from("no available upstream")),
        Err(DialError::Connect(err)) => return Err(format!("connect failed: {err}")),
    };

    let started = Instant::now();
    let address = selection.upstream.address();
    if group.dialer.is_tls() && !client.sets_host {
        proxy::set_backend_host(&mut request, address);
    }
    match group
        .transport
        .send(&selection.upstream, preface, connection, request)
        .await
    {
        Ok((response, _permit)) => {
            selection
                .upstream
                .record_response(response.status().as_u16(), started.elapsed());
            // Read to the end, without keeping it, so the connection is reused.
            let mut body = response.into_body();
            while let Some(frame) = body.frame().await {
                if let Err(err) = frame {
                    return Err(format!("upstream {address} failed: {err}"));
                }
            }
            Ok(())
        }
        // Not retried: the copy is best effort.
        Err(SendError::Stale(_)) => {
            group.transport.forget(address, preface);
            Err(format!("upstream {address} closed a pooled connection"))
        }
        Err(SendError::Failed(err)) => {
            selection.upstream.record_failure();
            Err(format!("upstream {address} failed: {err}"))
        }
    }
}
