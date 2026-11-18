//! Per-connection HTTP service: routes each request to the proxy or the file
//! server according to the server's `match` list.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hyper::Request;
use hyper::body::Incoming;
use rxh_core::config::{self, Action, ConfigError, RequestHeaders, ResponseHeaders};
use rxh_core::matcher::{MatchInput, RequestMatcher};
use rxh_core::net::IpNet;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::error::Error;
use crate::files;
use crate::http::cache::Cache;
use crate::http::proxy;
use crate::http::response::{self, BoxResponse};
use crate::registry::{Group, Registry};

/// Compiled HTTP server: routes plus the settings they share.
#[derive(Debug)]
pub(crate) struct Site {
    /// Name used in logs, like `127.0.0.1:80 (web)`.
    pub(crate) log_name: String,

    /// Proxy identifier for `Forwarded: by=` and `Via`.
    pub(crate) by: String,

    /// Routes, matched in order.
    pub(crate) routes: Vec<Route>,

    /// Peers allowed to set `X-Forwarded-*`.
    pub(crate) trusted_proxies: Vec<IpNet>,

    /// True if the listener terminates TLS.
    pub(crate) tls: bool,
}

/// One compiled pattern.
#[derive(Debug)]
pub(crate) struct Route {
    pub(crate) uri: String,
    pub(crate) matcher: RequestMatcher,
    pub(crate) action: RouteAction,
    pub(crate) request: RequestHeaders,
    pub(crate) response: ResponseHeaders,
    pub(crate) cache: Option<Cache>,
}

/// What a route does.
#[derive(Debug)]
pub(crate) enum RouteAction {
    /// Reverse proxy to a group.
    Proxy(Arc<Group>),

    /// Static files from a directory.
    Files(String),
}

impl Site {
    pub(crate) fn build(
        server_index: usize,
        server: &config::Server,
        registry: &Registry,
        log_name: String,
        local: SocketAddr,
    ) -> Result<Self, Error> {
        let mut routes = Vec::with_capacity(server.patterns.len());

        for (pattern_index, pattern) in server.patterns.iter().enumerate() {
            let action = match &pattern.action {
                Action::Forward(forward) => RouteAction::Proxy(registry.resolve(
                    forward,
                    server_index,
                    server,
                    pattern_index,
                )?),
                Action::Serve(root) => RouteAction::Files(root.clone()),
            };

            routes.push(Route {
                uri: pattern.uri.clone(),
                matcher: RequestMatcher::new(pattern, &server.host).map_err(|err| {
                    Error::Config(ConfigError::Invalid(format!(
                        "match[{pattern_index}]: {err}"
                    )))
                })?,
                action,
                request: pattern.request.headers.clone(),
                response: pattern.response.headers.clone(),
                cache: Cache::new(&pattern.cache),
            });
        }

        Ok(Self {
            log_name,
            by: server.name.clone().unwrap_or_else(|| local.to_string()),
            routes,
            trusted_proxies: server.trusted_proxies.clone(),
            tls: server.tls.is_some(),
        })
    }
}

/// Lets the connection task know that requests arrived, which matters for
/// graceful shutdown: idle keep-alive connections are closed, connections
/// that never sent a request are left alone until they do.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    pub(crate) requests: AtomicUsize,
    pub(crate) notify: Notify,
}

impl Activity {
    /// Waits until at least one request has been received.
    pub(crate) async fn first_request(&self) {
        while self.requests.load(Ordering::Acquire) == 0 {
            self.notify.notified().await;
        }
    }
}

/// [`hyper::service::Service`] of one client connection.
#[derive(Clone)]
pub(crate) struct Service {
    pub(crate) site: Arc<Site>,
    pub(crate) peer: SocketAddr,
    pub(crate) local: SocketAddr,
    pub(crate) activity: Arc<Activity>,
}

impl hyper::service::Service<Request<Incoming>> for Service {
    type Response = BoxResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        self.activity.requests.fetch_add(1, Ordering::Release);
        self.activity.notify.notify_one();

        let service = self.clone();
        Box::pin(async move { Ok(service.handle(request).await) })
    }
}

impl Service {
    async fn handle(self, request: Request<Incoming>) -> BoxResponse {
        let started = Instant::now();
        let method = request.method().clone();
        let uri = request.uri().clone();
        let path = uri.path().to_owned();

        let input = MatchInput {
            method: &method,
            uri: &uri,
            headers: request.headers(),
            peer: self.peer.ip(),
        };
        let route = self
            .site
            .routes
            .iter()
            .find(|route| route.matcher.matches(&input));

        let response = match route {
            None => response::not_found(),
            Some(route) => match &route.action {
                RouteAction::Proxy(group) => proxy::forward(request, route, group, &self).await,
                RouteAction::Files(root) => files::transfer(&path, root, &route.uri).await,
            },
        };

        println!(
            "{} -> {} {method} {uri} HTTP {} {:?}",
            self.peer,
            self.site.log_name,
            response.status(),
            started.elapsed()
        );

        response
    }
}
