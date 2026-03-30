//! Proxy requests require additional information attached to them such as
//! the IP address of the client, which we can't obtain using [`hyper::Request`]
//! alone. We also need to implement different RFCs for request forwarding.

use std::net::SocketAddr;

use http::HeaderMap;
use hyper::{
    Method,
    Uri,
    Version,
    Request,
    header::{self, HeaderValue},
    upgrade::OnUpgrade,
};

use crate::config;

/// Request received by this proxy from a client.
pub(crate) struct ProxyRequest<T> {
    /// Original client request.
    request: Request<T>,

    /// Client socket.
    client_addr: SocketAddr,

    /// Local socket currently handling this request.
    server_addr: SocketAddr,

    /// Optional ID to use in the "by" parameter of the "Forwarded" header
    /// instead of the IP address.
    proxy_id: Option<String>,

    /// Configurable request header behavior.
    headers_config: config::RequestHeaders,
}

impl<T> ProxyRequest<T> {
    /// Creates a new [`ProxyRequest`].
    pub fn new(
        request: Request<T>,
        client_addr: SocketAddr,
        server_addr: SocketAddr,
        proxy_id: Option<String>,
        headers_config: config::RequestHeaders,
    ) -> Self {
        Self {
            request,
            client_addr,
            server_addr,
            proxy_id,
            headers_config,
        }
    }

    pub fn headers(&self) -> &HeaderMap {
        self.request.headers()
    }

    pub fn method(&self) -> &Method {
        self.request.method()
    }

    pub fn uri(&self) -> &Uri {
        self.request.uri()
    }

    pub fn version(&self) -> Version {
        self.request.version()
    }

    /// Extracts the request upgrade future.
    pub fn on_upgrade(&mut self) -> OnUpgrade {
        hyper::upgrade::on(&mut self.request)
    }

    /// Consumes the [`ProxyRequest`] returning a [`hyper::Request`] that
    /// contains a valid HTTP forwarded header. This is an implementation of
    /// RFC 7239, see the details in the section below.
    ///
    /// # RFC 7239
    ///
    /// Document: https://www.rfc-editor.org/rfc/rfc7239
    ///
    /// ## Summary
    ///
    /// The HTTP `Forwarded` header allows proxies to disclose information lost
    /// in the proxying process, such as the IP of the original client. This
    /// header is optional.
    ///
    /// ### HTTP `Forwarded` header format:
    ///
    /// ```text
    /// Forwarded: for={};by={};host={};proto={}
    /// ```
    ///
    /// ### HTTP `Forwarded` header parameters:
    ///
    /// - `for`: Identifies the client that initiated the request. This may be
    ///   an obfuscated ID or IP + optional TCP port.
    ///
    /// - `by`: Identifies the interface where the request came in to the proxy
    ///   server. This may be an obfuscated ID, IP or IP and TCP port.
    ///
    /// - `host`: Original value of the `Host` HTTP header, as received by the
    ///   proxy.
    ///
    /// - `proto`: Protocol used to make the request. For example, `http` or
    ///   `https`.
    ///
    /// All parameters are optional. IP addresses may be IPv4 or IPv6. If the
    /// address is IPv6 it must be enclosed in square brackets.
    ///
    /// ### HTTP Lists
    ///
    /// If the request passes through multiple proxies, this header could
    /// contain a list of values separated by commas.
    ///
    /// ### Example
    ///
    /// A request from a client with IP address 192.0.2.43 passes through a
    /// proxy with IP address 198.51.100.17, then through another proxy with IP
    /// address 203.0.113.60 before reaching an origin server.
    ///
    /// The HTTP request between the client and the first proxy has no
    /// `Forwarded` header field.
    ///
    /// The HTTP request between the first and second proxy has a
    /// `Forwarded: for=192.0.2.43` header field.
    ///
    /// The HTTP request between the second proxy and the origin server
    /// contains:
    ///
    /// ```text
    /// Forwarded: for=192.0.2.43, for=198.51.100.17;by=203.0.113.60;proto=http;host=example.com
    /// ```
    ///
    /// ### Security Considerations
    ///
    /// There is nothing that can be trusted in this header, as every proxy
    /// in the chain can manipulate the value. Even the original client can
    /// set any value to the `Forwarded` header.
    pub fn into_forwarded(mut self) -> Request<T> {
        let host = if let Some(value) = self.request.headers().get(header::HOST) {
            match value.to_str() {
                Ok(host) => String::from(host),
                Err(_) => self.server_addr.to_string(),
            }
        } else {
            self.server_addr.to_string()
        };

        let by = self
            .headers_config
            .forwarded
            .by
            .clone()
            .or(self.proxy_id.clone())
            .unwrap_or(self.server_addr.to_string());

        let proto = if self.request.version() == Version::HTTP_2 {
            "https"
        } else {
            "http"
        };

        let mut forwarded = format!("for={};by={};host={};proto={}", self.client_addr, by, host, proto);

        if self.headers_config.forwarded.extend
            && let Some(previous) = combine_header_values(self.request.headers(), header::FORWARDED)
        {
            forwarded = format!("{previous}, {forwarded}");
        }

        self.request.headers_mut().insert(
            header::FORWARDED,
            HeaderValue::from_str(&forwarded).unwrap(),
        );

        let via_member = format!("{} {}", version_token(self.request.version()), by);
        append_list_header(self.request.headers_mut(), header::VIA, &via_member);

        for (name, value) in &self.headers_config.set {
            if let Ok(name) = header::HeaderName::from_bytes(name.as_bytes())
                && let Ok(value) = HeaderValue::from_str(value)
            {
                self.request.headers_mut().insert(name, value);
            }
        }

        for name in &self.headers_config.remove {
            if let Ok(name) = header::HeaderName::from_bytes(name.as_bytes()) {
                self.request.headers_mut().remove(name);
            }
        }

        self.request
    }
}

fn version_token(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_11 => "1.1",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "1.1",
    }
}

fn combine_header_values(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    let mut values = vec![];

    for value in headers.get_all(name) {
        if let Ok(value) = value.to_str() {
            values.push(value.trim().to_string());
        }
    }

    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

fn append_list_header(headers: &mut HeaderMap, name: header::HeaderName, value: &str) {
    let new_value = if let Some(previous) = combine_header_values(headers, name.clone()) {
        format!("{previous}, {value}")
    } else {
        value.to_string()
    };

    headers.insert(name, HeaderValue::from_str(&new_value).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded_header<T>(request: &Request<T>) -> String {
        let forwarded = request
            .headers()
            .get(header::FORWARDED)
            .unwrap()
            .to_str()
            .unwrap();

        String::from(forwarded)
    }

    #[test]
    fn forwarded_request() {
        let client = "127.0.0.1:8000".parse().unwrap();
        let proxy = "127.0.0.1:9000".parse().unwrap();

        let request = ProxyRequest::new(
            Request::builder().body(crate::http::body::empty()).unwrap(),
            client,
            proxy,
            None,
            Default::default(),
        );

        let forwarded = request.into_forwarded();
        let expected = format!("for={client};by={proxy};host={proxy};proto=http");

        assert!(forwarded.headers().contains_key(header::FORWARDED));
        assert_eq!(forwarded_header(&forwarded), expected.as_str());
    }

    #[test]
    fn forwarded_request_with_proxy_id() {
        let client = "127.0.0.1:8000".parse().unwrap();
        let proxy = "127.0.0.1:9000".parse().unwrap();
        let proxy_id = String::from("rxh/main");

        let request = ProxyRequest::new(
            Request::builder().body(crate::http::body::empty()).unwrap(),
            client,
            proxy,
            Some(proxy_id.clone()),
            Default::default(),
        );

        let forwarded = request.into_forwarded();
        let expected = format!("for={client};by={proxy_id};host={proxy};proto=http");

        assert!(forwarded.headers().contains_key(header::FORWARDED));
        assert_eq!(forwarded_header(&forwarded), expected.as_str());
    }
}
