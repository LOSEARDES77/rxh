//! Per-connection HTTP service: routes each request to the proxy or the file
//! server according to the server's `match` list.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::Request;
use hyper::body::Incoming;
use rxh_core::config::{self, Action, ConfigError, RequestHeaders, ResponseHeaders};
use rxh_core::matcher::{MatchInput, RequestMatcher};
use rxh_core::net::IpNet;
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::error::Error;
use crate::files;
use crate::http::body::Guarded;
use crate::http::cache::Cache;
use crate::http::headers::{self, Replacement};
use crate::http::proxy;
use crate::http::response::{self, BoxResponse};
use crate::log::{self, Logger};
use crate::metrics;
use crate::registry::{Group, Registry};

/// Settings of one `[[server]]`, shared by its routes. Several sites can
/// share one listener, each route keeps its own site.
#[derive(Debug)]
pub(crate) struct Site {
    /// Name used in logs, like `127.0.0.1:80 (web)`.
    pub(crate) log_name: String,

    /// Proxy identifier for `Forwarded: by=` and `Via`.
    pub(crate) by: String,

    /// Peers allowed to set `X-Forwarded-*`.
    pub(crate) trusted_proxies: Vec<IpNet>,

    /// True if the listener terminates TLS.
    pub(crate) tls: bool,

    /// Maximum lifetime of upgraded tunnels.
    pub(crate) stream_timeout: Option<Duration>,
}

/// One compiled pattern.
#[derive(Debug)]
pub(crate) struct Route {
    pub(crate) site: Arc<Site>,
    pub(crate) uri: String,
    pub(crate) matcher: RequestMatcher,
    pub(crate) action: RouteAction,
    pub(crate) request: RequestHeaders,
    pub(crate) response: ResponseHeaders,
    pub(crate) request_replace: Vec<Replacement>,
    pub(crate) response_replace: Vec<Replacement>,
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
    /// Compiles the routes of `server`, appending them to `routes`.
    pub(crate) fn build_routes(
        server_index: usize,
        server: &config::Server,
        registry: &Registry,
        log_name: String,
        local: SocketAddr,
        routes: &mut Vec<Route>,
    ) -> Result<(), Error> {
        let site = Arc::new(Self {
            log_name,
            by: server.name.clone().unwrap_or_else(|| local.to_string()),
            trusted_proxies: server.trusted_proxies.clone(),
            tls: server.tls.is_some(),
            stream_timeout: server.stream_timeout,
        });

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

            let invalid = |side: &str, err: String| {
                Error::Config(ConfigError::Invalid(format!(
                    "match[{pattern_index}]: {side}.headers: {err}"
                )))
            };
            let request_replace = Replacement::compile(&pattern.request.headers.replace)
                .map_err(|err| invalid("request", err))?;
            let response_replace = Replacement::compile(&pattern.response.headers.replace)
                .map_err(|err| invalid("response", err))?;

            routes.push(Route {
                site: Arc::clone(&site),
                uri: pattern.uri.clone(),
                matcher: RequestMatcher::new(pattern, &server.host).map_err(|err| {
                    Error::Config(ConfigError::Invalid(format!(
                        "match[{pattern_index}]: {err}"
                    )))
                })?,
                action,
                request: pattern.request.headers.clone(),
                response: pattern.response.headers.clone(),
                request_replace,
                response_replace,
                cache: Cache::new(&pattern.cache),
            });
        }

        Ok(())
    }
}

/// Lets the connection task know that requests arrived, which matters for
/// graceful shutdown: idle keep-alive connections are closed, connections
/// that never sent a request are left alone until they do.
#[derive(Debug, Default)]
pub(crate) struct Activity {
    pub(crate) requests: AtomicUsize,
    in_flight: AtomicUsize,
    pub(crate) notify: Notify,
}

/// Counts a request as in flight until dropped.
pub(crate) struct InFlight(Arc<Activity>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::Release);
    }
}

impl Activity {
    /// Requests whose response has not been fully sent yet.
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

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
    /// Routes of every server sharing the listener, in configuration order.
    pub(crate) routes: Arc<[Route]>,
    /// Name of the listener, for requests that match no route.
    pub(crate) log_name: Arc<str>,
    pub(crate) peer: SocketAddr,
    pub(crate) local: SocketAddr,
    pub(crate) activity: Arc<Activity>,
    pub(crate) logger: Arc<Logger>,
}

/// Response extension set by the proxy: address of the backend that served it.
#[derive(Debug, Clone)]
pub(crate) struct UpstreamAddress(pub(crate) String);

impl hyper::service::Service<Request<Incoming>> for Service {
    type Response = BoxResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        self.activity.requests.fetch_add(1, Ordering::Release);
        self.activity.notify.notify_one();

        self.activity.in_flight.fetch_add(1, Ordering::Release);
        let in_flight = InFlight(Arc::clone(&self.activity));

        let service = self.clone();
        Box::pin(async move {
            let response = service.handle(request).await;
            Ok(response.map(|body| Guarded::new(body, in_flight).boxed()))
        })
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
            .routes
            .iter()
            .find(|route| route.matcher.matches(&input));

        let server = route.map_or(&*self.log_name, |route| &*route.site.by);
        // Only computed when a JSON line will be written.
        let json = self.logger.is_json();
        let client_ip = match route {
            Some(route) if json => {
                headers::client_ip(
                    self.peer.ip(),
                    request.headers(),
                    &route.site.trusted_proxies,
                )
                .0
            }
            _ => self.peer.ip(),
        };
        let version = request.version();

        let mut response = match route {
            None => response::not_found(),
            Some(route) => match &route.action {
                RouteAction::Proxy(group) => proxy::forward(request, route, group, &self).await,
                RouteAction::Files(root) => files::transfer(&path, root, &route.uri).await,
            },
        };

        let elapsed = started.elapsed();
        let status = response.status().as_u16();
        let upstream = response
            .extensions_mut()
            .remove::<UpstreamAddress>()
            .map(|address| address.0);
        metrics::global().record_http(server, status, elapsed);
        self.logger.access(
            || {
                format!(
                    "{} -> {} {method} {uri} HTTP {} {elapsed:?}",
                    self.peer,
                    route.map_or(&*self.log_name, |route| &route.site.log_name),
                    response.status(),
                )
            },
            || {
                serde_json::json!({
                    "ts": log::now(),
                    "server": server,
                    "remote": self.peer.to_string(),
                    "client_ip": client_ip.to_string(),
                    "method": method.as_str(),
                    "uri": uri.to_string(),
                    "proto": format!("{version:?}"),
                    "status": status,
                    "duration_ms": log::millis(elapsed),
                    "upstream": upstream,
                })
            },
        );

        response
    }
}
