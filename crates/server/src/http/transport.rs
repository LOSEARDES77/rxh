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

use crate::http::body::Body;
use crate::stream::Dialer;

type IdleMap = HashMap<String, Vec<Idle>>;

#[derive(Debug)]
struct Idle {
    sender: http1::SendRequest<Body>,
    since: Instant,
}

/// Connection ready to carry one request.
#[derive(Debug)]
pub(crate) enum Connection {
    /// HTTP/1.1, possibly taken from the idle pool.
    Http1 {
        sender: http1::SendRequest<Body>,
        reused: bool,
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
            idle: Arc::default(),
            http2: Mutex::default(),
        }
    }

    /// Returns a connection to `address`, reusing an idle one if possible.
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
                if let Err(err) = connection.await {
                    eprintln!("upstream h2c connection error: {err}");
                }
            });
            self.http2
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(address.to_owned(), sender.clone());
            return Ok(Connection::Http2(sender));
        }

        if let Some(sender) = self.take_idle(address) {
            return Ok(Connection::Http1 {
                sender,
                reused: true,
            });
        }

        Ok(Connection::Http1 {
            sender: self.dial_http1(address).await?,
            reused: false,
        })
    }

    async fn dial_http1(&self, address: &str) -> io::Result<http1::SendRequest<Body>> {
        let stream = self.dialer.dial(address).await?;
        let (sender, connection) = http1::Builder::new()
            .preserve_header_case(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(io::Error::other)?;
        tokio::task::spawn(async move {
            if let Err(err) = connection.with_upgrades().await {
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
    /// been read completely.
    fn recycle(&self, address: &str, mut sender: http1::SendRequest<Body>) {
        if self.keepalive.is_zero() || self.max_idle == 0 {
            return;
        }

        let idle = Arc::clone(&self.idle);
        let address = address.to_owned();
        let (keepalive, max_idle) = (self.keepalive, self.max_idle);

        tokio::task::spawn(async move {
            if sender.ready().await.is_err() {
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

    /// Sends `request` over `connection`.
    pub(crate) async fn send(
        &self,
        address: &str,
        connection: Connection,
        request: Request<Body>,
    ) -> Result<Response<Incoming>, SendError> {
        match connection {
            Connection::Http2(mut sender) => sender
                .send_request(request)
                .await
                .map_err(|err| SendError::Failed(io::Error::other(err))),
            Connection::Http1 { mut sender, reused } => {
                match sender.try_send_request(request).await {
                    Ok(response) => {
                        self.recycle(address, sender);
                        Ok(response)
                    }
                    Err(mut err) => match err.take_message() {
                        Some(request) if reused => Err(SendError::Stale(Box::new(request))),
                        _ => Err(SendError::Failed(io::Error::other(err.into_error()))),
                    },
                }
            }
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
