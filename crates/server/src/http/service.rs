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
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::{Method, Request, StatusCode, Uri};
use rxh_core::config::{self, Action, ConfigError, Encoding, RequestHeaders, ResponseHeaders};
use rxh_core::matcher::{MatchInput, RequestMatcher};
use rxh_core::net::IpNet;
use rxh_core::pool::Upstream;
use rxh_core::template::Template;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::acme;
use crate::error::Error;
use crate::files::Files;
use crate::http::body::Guarded;
use crate::http::cache::Cache;
use crate::http::encode;
use crate::http::headers::{self, Replacement, Via};
use crate::http::proxy;
use crate::http::response::{self, BoxResponse};
use crate::http::response_handler::ResponseHandler;
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

    /// `Via` entries naming `by`.
    pub(crate) via: Via,

    /// Peers allowed to set `X-Forwarded-*`.
    pub(crate) trusted_proxies: Vec<IpNet>,

    /// True if the listener terminates TLS.
    pub(crate) tls: bool,

    /// PROXY protocol header sent on new connections to backends.
    pub(crate) proxy_protocol: Option<config::ProxyProtocol>,

    /// Maximum lifetime of upgraded tunnels.
    pub(crate) stream_timeout: Option<Duration>,

    /// `stream_close_delay`, and the flag the listener raises when it shuts
    /// down.
    pub(crate) stream_close: Option<(Duration, watch::Receiver<bool>)>,
}

/// One compiled pattern.
#[derive(Debug)]
pub(crate) struct Route {
    pub(crate) site: Arc<Site>,
    pub(crate) uri: String,
    pub(crate) matcher: RequestMatcher,
    pub(crate) action: RouteAction,
    /// New URI of the request, see [`rewritten`].
    pub(crate) rewrite: Option<Template>,
    /// Method the request is forwarded or served with.
    pub(crate) method: Option<Method>,
    /// Largest request body accepted, in bytes.
    pub(crate) max_body_size: Option<usize>,
    pub(crate) request: RequestHeaders,
    pub(crate) response: ResponseHeaders,
    pub(crate) request_replace: Vec<Replacement>,
    pub(crate) response_replace: Vec<Replacement>,
    /// `handle_response` entries; the first one matching an upstream
    /// response runs.
    pub(crate) handle_response: Vec<ResponseHandler>,
    pub(crate) cache: Option<Cache>,
    pub(crate) encode: Vec<Encoding>,
}

/// What a route does.
#[derive(Debug)]
pub(crate) enum RouteAction {
    /// Reverse proxy to a group.
    Proxy(Arc<Group>),

    /// Static files from a directory.
    Files(Files),

    /// Redirect to the rendered `to` with `status`.
    Redirect { to: Template, status: StatusCode },
}

/// URI of a request for `uri` on `host` after `rewrite`: scheme and
/// authority are kept, and so is the query unless the rewrite has its own.
/// `None` if the result is not a valid URI.
fn rewritten(rewrite: &Template, host: &str, uri: &Uri) -> Option<Uri> {
    let mut target = rewrite.render(host, uri);
    if let Some(query) = uri.query()
        && !target.contains('?')
    {
        target = format!("{target}?{query}");
    }
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = Some(target.parse().ok()?);
    Uri::from_parts(parts).ok()
}

impl Site {
    /// Compiles the routes of `server`, appending them to `routes`.
    pub(crate) fn build_routes(
        server_index: usize,
        server: &config::Server,
        registry: &Registry,
        log_name: String,
        local: SocketAddr,
        closing: &watch::Receiver<bool>,
        routes: &mut Vec<Route>,
    ) -> Result<(), Error> {
        let by = server.name.clone().unwrap_or_else(|| local.to_string());
        let site = Arc::new(Self {
            log_name,
            via: Via::new(&by),
            by,
            trusted_proxies: server.trusted_proxies.clone(),
            tls: server.tls.is_some(),
            proxy_protocol: server.proxy_protocol,
            stream_timeout: server.stream_timeout,
            stream_close: server
                .stream_close_delay
                .map(|delay| (delay, closing.clone())),
        });

        for (pattern_index, pattern) in server.patterns.iter().enumerate() {
            let invalid = |key: &str, err: String| {
                Error::Config(ConfigError::Invalid(format!(
                    "match[{pattern_index}]: {key}: {err}"
                )))
            };
            let action = match &pattern.action {
                Action::Forward(forward) => RouteAction::Proxy(registry.resolve(
                    forward,
                    server_index,
                    server,
                    pattern_index,
                )?),
                Action::Serve(root) => RouteAction::Files(Files::new(root)),
                Action::Redirect(redirect) => RouteAction::Redirect {
                    to: redirect
                        .to
                        .parse()
                        .map_err(|err| invalid("redirect", err))?,
                    status: StatusCode::from_u16(redirect.status)
                        .map_err(|err| invalid("redirect", err.to_string()))?,
                },
            };
            let rewrite: Option<Template> = pattern
                .rewrite
                .as_deref()
                .map(str::parse)
                .transpose()
                .map_err(|err| invalid("rewrite", err))?;
            let method: Option<Method> = pattern
                .request
                .method
                .as_ref()
                .map(|method| Method::from_bytes(method.to_ascii_uppercase().as_bytes()))
                .transpose()
                .map_err(|err| invalid("request.method", err.to_string()))?;

            let request_replace = Replacement::compile(&pattern.request.headers.replace)
                .map_err(|err| invalid("request.headers", err))?;
            let response_replace = Replacement::compile(&pattern.response.headers.replace)
                .map_err(|err| invalid("response.headers", err))?;
            let handle_response: Vec<ResponseHandler> = pattern
                .handle_response
                .iter()
                .map(ResponseHandler::compile)
                .collect::<Result<_, _>>()
                .map_err(|err| invalid("handle_response", err))?;

            routes.push(Route {
                site: Arc::clone(&site),
                uri: pattern.uri.clone(),
                matcher: RequestMatcher::new(pattern, &server.host).map_err(|err| {
                    Error::Config(ConfigError::Invalid(format!(
                        "match[{pattern_index}]: {err}"
                    )))
                })?,
                action,
                rewrite,
                method,
                max_body_size: pattern.request.max_body_size,
                request: pattern.request.headers.clone(),
                response: pattern.response.headers.clone(),
                request_replace,
                response_replace,
                handle_response,
                cache: Cache::new(&pattern.cache),
                encode: pattern.encode.clone(),
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

    /// Waits until at least one request has been received. Level triggered:
    /// every waiter returns, however many there are.
    pub(crate) async fn first_request(&self) {
        loop {
            let notified = self.notify.notified();
            let mut notified = std::pin::pin!(notified);
            // Registered before the check, so a `notify_waiters` between the
            // check and the await is not missed.
            notified.as_mut().enable();
            if self.requests.load(Ordering::Acquire) > 0 {
                return;
            }
            notified.await;
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
    /// `X-Forwarded-For` entry of `peer`.
    pub(crate) peer_ip: HeaderValue,
    pub(crate) local: SocketAddr,
    pub(crate) activity: Arc<Activity>,
    pub(crate) logger: Arc<Logger>,
}

/// Response extension set by the proxy for JSON access lines: the backend
/// that served the response.
#[derive(Debug, Clone)]
pub(crate) struct UpstreamAddress(pub(crate) Arc<Upstream>);

impl hyper::service::Service<Request<Incoming>> for Service {
    type Response = BoxResponse;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn call(&self, request: Request<Incoming>) -> Self::Future {
        self.activity.requests.fetch_add(1, Ordering::Release);
        self.activity.notify.notify_waiters();

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
    async fn handle(self, mut request: Request<Incoming>) -> BoxResponse {
        if let Some(response) = acme::challenge_response(request.method(), request.uri().path()) {
            return response;
        }

        let started = Instant::now();
        let method = request.method().clone();
        let uri = request.uri().clone();

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

        let accept_encoding = match route {
            Some(route) if !route.encode.is_empty() => encode::accept_encoding(request.headers()),
            _ => String::new(),
        };

        // The access log keeps the method and URI the client sent.
        let host = input.host().unwrap_or("");
        let location = match route.map(|route| &route.action) {
            Some(RouteAction::Redirect { to, .. }) => to.render(host, &uri),
            _ => String::new(),
        };
        let rewrite = route.and_then(|route| route.rewrite.as_ref());
        let rewrite = rewrite.map(|rewrite| rewritten(rewrite, host, &uri));
        let rewrite_failed = matches!(rewrite, Some(None));
        if let Some(Some(rewritten)) = rewrite {
            *request.uri_mut() = rewritten;
        }
        if let Some(method) = route.and_then(|route| route.method.as_ref()) {
            *request.method_mut() = method.clone();
        }

        // Declared length; bodies without one are counted as they stream.
        let declared = hyper::body::Body::size_hint(request.body()).lower();
        let too_large = route
            .and_then(|route| route.max_body_size)
            .is_some_and(|limit| declared > limit as u64);

        let mut response = match route {
            None => response::not_found(),
            Some(_) if rewrite_failed => {
                response::plain(StatusCode::BAD_REQUEST, "HTTP 400 BAD REQUEST")
            }
            Some(_) if too_large => response::payload_too_large(),
            Some(route) => match &route.action {
                RouteAction::Proxy(group) => proxy::forward(request, route, group, &self).await,
                RouteAction::Files(files) => {
                    files
                        .transfer(
                            request.method(),
                            request.headers(),
                            request.uri().path(),
                            &route.uri,
                        )
                        .await
                }
                RouteAction::Redirect { status, .. } => response::redirect(*status, &location),
            },
        };
        if let Some(route) = route
            && !route.encode.is_empty()
        {
            response = encode::apply(response, &route.encode, &accept_encoding);
        }

        let elapsed = started.elapsed();
        let status = response.status().as_u16();
        let upstream = response
            .extensions_mut()
            .remove::<UpstreamAddress>()
            .map(|upstream| upstream.0);
        metrics::global().record_http(server, status, elapsed);
        self.logger.access(
            |line| {
                log::push(
                    line,
                    format_args!(
                        "{} -> {} {method} {uri} HTTP {} {elapsed:?}",
                        self.peer,
                        route.map_or(&*self.log_name, |route| &route.site.log_name),
                        response.status(),
                    ),
                );
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
                    "upstream": upstream.as_deref().map(Upstream::address),
                })
            },
        );

        response
    }
}
