//! Upstream HTTP connections with keep-alive pooling.
//!
//! HTTP/1.1 connections are reused once the previous response body has been
//! fully read. With `h2c = true` a single multiplexed HTTP/2 connection is
//! shared per backend.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hyper::Request;
use hyper::Response;
use hyper::body::Incoming;
use hyper::client::conn::{http1, http2};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rxh_core::config::UpstreamGroup;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::http::body::Body;
use crate::stream::Dialer;

type IdleMap = HashMap<String, Vec<Idle>>;

#[derive(Debug)]
struct Idle {
    sender: http1::SendRequest<Body>,
    since: Instant,
}

/// Slot of `max_conns_per_host`, free again once every clone is dropped.
pub(crate) type Permit = Option<Arc<OwnedSemaphorePermit>>;

/// Connection ready to carry one request.
#[derive(Debug)]
pub(crate) enum Connection {
    /// HTTP/1.1, possibly taken from the idle pool.
    Http1 {
        sender: http1::SendRequest<Body>,
        reused: bool,
        permit: Permit,
    },

    /// Shared HTTP/2 connection.
    Http2(http2::SendRequest<Body>),
}

/// Why [`Transport::send`] failed.
#[derive(Debug)]
pub(crate) enum SendError {
    /// A pooled connection was closed before the request was written. The
    /// request is returned untouched and can go to any backend.
    Stale(Box<Request<Body>>),

    /// The request failed after it may have reached the backend.
    Failed(io::Error),
}

/// Connection pool for one load balancing group.
#[derive(Debug)]
pub(crate) struct Transport {
    dialer: Arc<Dialer>,
    keepalive: Duration,
    max_idle: usize,
    h2c: bool,
    max_conns: usize,
    /// One semaphore of `max_conns` permits per backend address.
    limits: Mutex<HashMap<String, Arc<Semaphore>>>,
    idle: Arc<Mutex<IdleMap>>,
    http2: Mutex<HashMap<String, http2::SendRequest<Body>>>,
}

impl Transport {
    pub(crate) fn new(settings: &UpstreamGroup, dialer: Arc<Dialer>) -> Self {
        Self {
            dialer,
            keepalive: settings.keepalive,
            max_idle: settings.keepalive_idle_conns_per_host,
            h2c: settings.h2c,
            max_conns: settings.max_conns_per_host,
            limits: Mutex::default(),
            idle: Arc::default(),
            http2: Mutex::default(),
        }
    }

    /// Returns a connection to `address`, reusing an idle one if possible.
    /// With `max_conns_per_host`, waits until the backend has a free slot.
    /// Errors here happen before the request is consumed, so they can be
    /// retried on another backend.
    pub(crate) async fn connect(&self, address: &str) -> io::Result<Connection> {
        if self.h2c {
            let existing = self
                .http2
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(address)
                .cloned();
            if let Some(sender) = existing
                && !sender.is_closed()
            {
                return Ok(Connection::Http2(sender));
            }

            let stream = self.dialer.dial(address).await?;
            let (sender, connection) = http2::Builder::new(TokioExecutor::new())
                .handshake(TokioIo::new(stream))
                .await
                .map_err(io::Error::other)?;
            tokio::task::spawn(async move {
                if let Err(err) = connection.await
                    && !is_idle_timeout(&err)
                {
                    eprintln!("upstream h2c connection error: {err}");
                }
            });
            self.http2
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(address.to_owned(), sender.clone());
            return Ok(Connection::Http2(sender));
        }

        let permit = self.acquire(address).await;
        if let Some(sender) = self.take_idle(address) {
            return Ok(Connection::Http1 {
                sender,
                reused: true,
                permit,
            });
        }

        Ok(Connection::Http1 {
            sender: self.dial_http1(address).await?,
            reused: false,
            permit,
        })
    }

    /// Waits for one of the `max_conns_per_host` slots of `address`.
    async fn acquire(&self, address: &str) -> Permit {
        if self.max_conns == 0 {
            return None;
        }
        let semaphore = Arc::clone(
            self.limits
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(address.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(self.max_conns))),
        );
        let permit = semaphore
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        Some(Arc::new(permit))
    }

    async fn dial_http1(&self, address: &str) -> io::Result<http1::SendRequest<Body>> {
        let stream = self.dialer.dial(address).await?;
        let (sender, connection) = http1::Builder::new()
            .preserve_header_case(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        tokio::task::spawn(async move {
            if let Err(err) = connection.with_upgrades().await
                && !is_idle_timeout(&err)
            {
                eprintln!("upstream connection error: {err}");
            }
        });
        Ok(sender)
    }

    fn take_idle(&self, address: &str) -> Option<http1::SendRequest<Body>> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let list = idle.get_mut(address)?;
        while let Some(entry) = list.pop() {
            if entry.since.elapsed() < self.keepalive && entry.sender.is_ready() {
                return Some(entry.sender);
            }
        }
        None
    }

    /// Puts the connection back in the idle pool once the response body has
    /// been read completely. `permit` is held until then, or until the
    /// connection closes.
    fn recycle(&self, address: &str, mut sender: http1::SendRequest<Body>, permit: Permit) {
        let pooled = !self.keepalive.is_zero() && self.max_idle > 0;
        if !pooled && permit.is_none() {
            return;
        }

        let idle = Arc::clone(&self.idle);
        let address = address.to_owned();
        let (keepalive, max_idle) = (self.keepalive, self.max_idle);

        tokio::task::spawn(async move {
            // Dropped last, after the connection is idle or closed.
            let _permit = permit;
            if sender.ready().await.is_err() || !pooled {
                return;
            }
            let mut idle = idle.lock().unwrap_or_else(PoisonError::into_inner);
            let list = idle.entry(address).or_default();
            list.retain(|entry| entry.since.elapsed() < keepalive && !entry.sender.is_closed());
            if list.len() < max_idle {
                list.push(Idle {
                    sender,
                    since: Instant::now(),
                });
            }
        });
    }

    /// Sends `request` over `connection`. The caller keeps the returned
    /// permit until it is done with the response body or the upgraded tunnel.
    pub(crate) async fn send(
        &self,
        address: &str,
        connection: Connection,
        request: Request<Body>,
    ) -> Result<(Response<Incoming>, Permit), SendError> {
        match connection {
            Connection::Http2(mut sender) => match sender.send_request(request).await {
                Ok(response) => Ok((response, None)),
                Err(err) => Err(SendError::Failed(io::Error::other(err))),
            },
            Connection::Http1 {
                mut sender,
                reused,
                permit,
            } => match sender.try_send_request(request).await {
                Ok(response) => {
                    self.recycle(address, sender, permit.clone());
                    Ok((response, permit))
                }
                Err(mut err) => match err.take_message() {
                    Some(request) if reused => Err(SendError::Stale(Box::new(request))),
                    _ => Err(SendError::Failed(io::Error::other(err.into_error()))),
                },
            },
        }
    }

    /// Drops idle connections to `address`, after one of them was found dead.
    pub(crate) fn forget(&self, address: &str) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(address);
    }

    /// True when requests must be HTTP/2 with an absolute URI.
    pub(crate) fn is_h2c(&self) -> bool {
        self.h2c
    }
}

/// True when `err` was caused by the upstream `read_timeout` firing, which
/// is how idle pooled connections end.
fn is_idle_timeout(err: &hyper::Error) -> bool {
    let mut source = std::error::Error::source(err);
    while let Some(inner) = source {
        if let Some(io) = inner.downcast_ref::<io::Error>()
            && io.kind() == io::ErrorKind::TimedOut
        {
            return true;
        }
        source = inner.source();
    }
    false
}
