//! `cors`: preflight answers and `Access-Control-*` response headers.

use hyper::header::{self, HeaderValue};
use hyper::{HeaderMap, Method, Response, StatusCode};
use rxh_core::config;

use crate::http::body;
use crate::http::response::{self, BoxResponse};

/// Runtime form of one `cors` table, with its header values built once.
#[derive(Debug)]
pub(crate) struct Cors {
    config: config::Cors,

    /// `Access-Control-Allow-Origin: *` instead of the request's origin.
    any_origin: bool,

    methods: HeaderValue,

    /// `None` allows the headers the preflight asks for.
    headers: Option<HeaderValue>,

    expose_headers: Option<HeaderValue>,

    max_age: Option<HeaderValue>,
}

/// Comma separated list, or `None` if `items` is empty.
fn list(items: &[String]) -> Option<HeaderValue> {
    if items.is_empty() {
        return None;
    }
    let value = HeaderValue::from_str(&items.join(", "))
        .expect("the configuration parser checked methods and header names");
    Some(value)
}

impl Cors {
    pub(crate) fn new(config: &config::Cors) -> Self {
        Self {
            any_origin: !config.credentials && config.origins.iter().any(|origin| origin == "*"),
            methods: list(&config.methods).expect("the configuration parser rejects empty methods"),
            headers: list(&config.headers),
            expose_headers: list(&config.expose_headers),
            max_age: config.max_age.map(|age| HeaderValue::from(age.as_secs())),
            config: config.clone(),
        }
    }

    /// Returns true if `request` is a CORS preflight, which RXH answers
    /// itself with [`Cors::preflight`].
    pub(crate) fn is_preflight(method: &Method, request: &HeaderMap) -> bool {
        method == Method::OPTIONS
            && request.contains_key(header::ORIGIN)
            && request.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
    }

    /// `204` answer to a preflight. Without the allow headers when the
    /// origin is not allowed, so the browser blocks the request.
    pub(crate) fn preflight(&self, request: &HeaderMap) -> BoxResponse {
        let mut response = Response::new(body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(header::SERVER, response::server_header(true));
        if !self.any_origin {
            headers.append(header::VARY, HeaderValue::from_static("Origin"));
        }
        headers.append(
            header::VARY,
            HeaderValue::from_static(
                "Access-Control-Request-Method, Access-Control-Request-Headers",
            ),
        );
        let Some(origin) = self.allowed_origin(request) else {
            return response;
        };
        headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        if self.config.credentials {
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, self.methods.clone());
        let allowed_headers = match &self.headers {
            Some(allowed) => Some(allowed.clone()),
            None => request.get(header::ACCESS_CONTROL_REQUEST_HEADERS).cloned(),
        };
        if let Some(allowed) = allowed_headers {
            headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, allowed);
        }
        if let Some(max_age) = &self.max_age {
            headers.insert(header::ACCESS_CONTROL_MAX_AGE, max_age.clone());
        }
        response
    }

    /// Sets the `Access-Control-*` headers of the answer to a request with
    /// `origin`. The ones of the backend are dropped: the route owns CORS.
    pub(crate) fn apply(&self, origin: Option<&HeaderValue>, response: &mut HeaderMap) {
        response.remove(header::ACCESS_CONTROL_ALLOW_ORIGIN);
        response.remove(header::ACCESS_CONTROL_ALLOW_CREDENTIALS);
        response.remove(header::ACCESS_CONTROL_EXPOSE_HEADERS);
        if !self.any_origin {
            // Caches must not serve one origin's answer to another.
            response.append(header::VARY, HeaderValue::from_static("Origin"));
        }
        let Some(origin) = origin.and_then(|origin| self.allows(origin)) else {
            return;
        };
        response.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        if self.config.credentials {
            response.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                HeaderValue::from_static("true"),
            );
        }
        if let Some(expose) = &self.expose_headers {
            response.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, expose.clone());
        }
    }

    fn allowed_origin(&self, request: &HeaderMap) -> Option<HeaderValue> {
        self.allows(request.get(header::ORIGIN)?)
    }

    /// `Access-Control-Allow-Origin` value for `origin`, if allowed.
    fn allows(&self, origin: &HeaderValue) -> Option<HeaderValue> {
        if self.any_origin {
            return Some(HeaderValue::from_static("*"));
        }
        let text = origin.to_str().ok()?;
        self.config.allows(text).then(|| origin.clone())
    }
}
