//! Header rewriting between client and upstream.
//!
//! Requests get hop-by-hop headers removed (RFC 9110 section 7.6.1), the
//! `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` headers set
//! like Caddy does, an RFC 7239 `Forwarded` header and a `Via` entry. Client
//! supplied `X-Forwarded-*` values are only kept when the peer is listed in
//! `trusted_proxies`.

use std::net::{IpAddr, SocketAddr};

use hyper::HeaderMap;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::Version;
use hyper::header::{self, HeaderName, HeaderValue};
use rxh_core::config::{RequestHeaders, ResponseHeaders};
use rxh_core::net::IpNet;

use crate::http::response;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

const HOP_BY_HOP: [HeaderName; 9] = [
    header::CONNECTION,
    HeaderName::from_static("proxy-connection"),
    HeaderName::from_static("keep-alive"),
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Connection facts needed to rewrite a request.
pub(crate) struct Forwarding<'a> {
    /// Directly connected peer.
    pub peer: SocketAddr,

    /// Local socket that accepted the connection.
    pub local: SocketAddr,

    /// True if `peer` is a trusted proxy.
    pub trusted: bool,

    /// True if the client connection uses TLS.
    pub tls: bool,

    /// Proxy identifier for `Forwarded: by=` and `Via`.
    pub by: &'a str,

    /// User header rules.
    pub rules: &'a RequestHeaders,
}

/// Real client IP: the left-most `X-Forwarded-For` entry when the peer is a
/// trusted proxy, otherwise the peer itself. Returns the IP and whether the
/// peer is trusted.
pub(crate) fn client_ip(peer: IpAddr, headers: &HeaderMap, trusted: &[IpNet]) -> (IpAddr, bool) {
    if !trusted.iter().any(|net| net.contains(peer)) {
        return (peer, false);
    }

    let forwarded = headers
        .get(&X_FORWARDED_FOR)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .and_then(|first| first.trim().parse().ok());

    (forwarded.unwrap_or(peer), true)
}

/// Returns the `Upgrade` value if the client asked for a protocol upgrade.
pub(crate) fn requested_upgrade(headers: &HeaderMap) -> Option<HeaderValue> {
    let wants_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|token| token.trim().eq_ignore_ascii_case("upgrade"));
    if wants_upgrade {
        headers.get(header::UPGRADE).cloned()
    } else {
        None
    }
}

/// Removes hop-by-hop headers, including those listed in `Connection`.
pub(crate) fn remove_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();

    for name in listed {
        headers.remove(name);
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Rewrites the headers of a request about to be sent upstream.
pub(crate) fn prepare_request(
    headers: &mut HeaderMap,
    version: Version,
    uri: &Uri,
    client_ip: IpAddr,
    forwarding: &Forwarding<'_>,
) {
    let wants_trailers = headers
        .get(header::TE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("trailers"));
    let upgrade = requested_upgrade(headers);

    remove_hop_by_hop(headers);

    if wants_trailers {
        headers.insert(header::TE, HeaderValue::from_static("trailers"));
    }
    if let Some(upgrade) = upgrade {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, upgrade);
    }

    // HTTP/2 clients send `:authority` instead of `Host`.
    if !headers.contains_key(header::HOST)
        && let Some(authority) = uri.authority()
        && let Ok(value) = HeaderValue::from_str(authority.as_str())
    {
        headers.insert(header::HOST, value);
    }

    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| forwarding.local.to_string());
    let proto = if forwarding.tls { "https" } else { "http" };

    set_x_forwarded(headers, client_ip, forwarding, &host, proto);
    set_forwarded(headers, forwarding, &host, proto);

    let via = format!("{} {}", version_token(version), forwarding.by);
    append_list(headers, header::VIA, &via);

    for (name, value) in &forwarding.rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for name in &forwarding.rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

fn set_x_forwarded(
    headers: &mut HeaderMap,
    client_ip: IpAddr,
    forwarding: &Forwarding<'_>,
    host: &str,
    proto: &str,
) {
    let peer = forwarding.peer.ip().to_canonical().to_string();

    if forwarding.trusted {
        append_list(headers, X_FORWARDED_FOR, &peer);
    } else {
        insert(
            headers,
            X_FORWARDED_FOR,
            &client_ip.to_canonical().to_string(),
        );
    }

    if !(forwarding.trusted && headers.contains_key(&X_FORWARDED_PROTO)) {
        insert(headers, X_FORWARDED_PROTO, proto);
    }
    if !(forwarding.trusted && headers.contains_key(&X_FORWARDED_HOST)) {
        insert(headers, X_FORWARDED_HOST, host);
    }
}

/// RFC 7239 `Forwarded` header.
fn set_forwarded(headers: &mut HeaderMap, forwarding: &Forwarding<'_>, host: &str, proto: &str) {
    let by = forwarding
        .rules
        .forwarded
        .by
        .as_deref()
        .unwrap_or(forwarding.by);
    let element = format!(
        "for={};by={};host={};proto={proto}",
        quote(&node(forwarding.peer)),
        quote(by),
        quote(host)
    );

    if forwarding.rules.forwarded.extend {
        append_list(headers, header::FORWARDED, &element);
    } else {
        insert(headers, header::FORWARDED, &element);
    }
}

/// Node identifier of RFC 7239 section 6: IPv6 goes in brackets.
fn node(address: SocketAddr) -> String {
    match address.ip().to_canonical() {
        IpAddr::V4(ip) => format!("{ip}:{}", address.port()),
        IpAddr::V6(ip) => format!("[{ip}]:{}", address.port()),
    }
}

/// Quotes a value unless it is a plain token (RFC 9110 section 5.6.2).
fn quote(value: &str) -> String {
    let is_token = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if is_token {
        value.to_owned()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Rewrites the headers of an upstream response before it goes to the client.
pub(crate) fn prepare_response<B>(
    response: &mut Response<B>,
    rules: &ResponseHeaders,
    received_by: &str,
) {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        remove_hop_by_hop(response.headers_mut());
    }

    let via = format!("{} {received_by}", version_token(response.version()));
    let headers = response.headers_mut();

    if rules.server.should_override {
        insert(
            headers,
            header::SERVER,
            &response::server_header(rules.server.version),
        );
    }
    if rules.via.extend {
        append_list(headers, header::VIA, &via);
    }
    for (name, value) in &rules.set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for name in &rules.remove {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }
}

fn version_token(version: Version) -> &'static str {
    match version {
        Version::HTTP_09 => "0.9",
        Version::HTTP_10 => "1.0",
        Version::HTTP_2 => "2",
        Version::HTTP_3 => "3",
        _ => "1.1",
    }
}

fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

/// Appends `value` to a comma separated list header.
fn append_list(headers: &mut HeaderMap, name: HeaderName, value: &str) {
    let previous: Vec<&str> = headers
        .get_all(&name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::trim)
        .collect();

    let combined = if previous.is_empty() {
        value.to_owned()
    } else {
        format!("{}, {value}", previous.join(", "))
    };
    insert(headers, name, &combined);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarding(rules: &RequestHeaders, trusted: bool) -> Forwarding<'_> {
        Forwarding {
            peer: "10.0.0.2:5000".parse().unwrap(),
            local: "127.0.0.1:8080".parse().unwrap(),
            trusted,
            tls: false,
            by: "edge",
            rules,
        }
    }

    #[test]
    fn untrusted_peer_cannot_spoof_client_ip() {
        let rules = RequestHeaders::default();
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, "1.2.3.4".parse().unwrap());
        headers.insert(header::HOST, "example.com".parse().unwrap());

        let peer: IpAddr = "10.0.0.2".parse().unwrap();
        let (client, trusted) = client_ip(peer, &headers, &[]);
        assert_eq!(client, peer);
        prepare_request(
            &mut headers,
            Version::HTTP_11,
            &Uri::from_static("/"),
            client,
            &forwarding(&rules, trusted),
        );

        assert_eq!(headers[&X_FORWARDED_FOR], "10.0.0.2");
        assert_eq!(headers[&X_FORWARDED_PROTO], "http");
        assert_eq!(headers[&X_FORWARDED_HOST], "example.com");
        assert_eq!(
            headers[header::FORWARDED],
            "for=\"10.0.0.2:5000\";by=edge;host=example.com;proto=http"
        );
        assert_eq!(headers[header::VIA], "1.1 edge");
    }

    #[test]
    fn trusted_peer_extends_chain() {
        let rules = RequestHeaders::default();
        let trusted_nets = IpNet::private_ranges();
        let mut headers = HeaderMap::new();
        headers.insert(X_FORWARDED_FOR, "1.2.3.4".parse().unwrap());

        let (client, trusted) = client_ip("10.0.0.2".parse().unwrap(), &headers, &trusted_nets);
        assert_eq!(client, "1.2.3.4".parse::<IpAddr>().unwrap());
        prepare_request(
            &mut headers,
            Version::HTTP_11,
            &Uri::from_static("/"),
            client,
            &forwarding(&rules, trusted),
        );
        assert_eq!(headers[&X_FORWARDED_FOR], "1.2.3.4, 10.0.0.2");
    }

    #[test]
    fn hop_by_hop_headers_are_removed_but_upgrades_survive() {
        let rules = RequestHeaders::default();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONNECTION,
            "keep-alive, x-secret, upgrade".parse().unwrap(),
        );
        headers.insert("x-secret", "1".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());
        headers.insert(header::TE, "trailers, deflate".parse().unwrap());

        prepare_request(
            &mut headers,
            Version::HTTP_11,
            &Uri::from_static("/"),
            "10.0.0.2".parse().unwrap(),
            &forwarding(&rules, false),
        );

        assert!(!headers.contains_key("x-secret"));
        assert!(!headers.contains_key("keep-alive"));
        assert_eq!(headers[header::CONNECTION], "upgrade");
        assert_eq!(headers[header::UPGRADE], "websocket");
        assert_eq!(headers[header::TE], "trailers");
    }

    #[test]
    fn http2_authority_becomes_host() {
        let rules = RequestHeaders::default();
        let mut headers = HeaderMap::new();
        let uri = Uri::from_static("https://example.com:8443/path");
        prepare_request(
            &mut headers,
            Version::HTTP_2,
            &uri,
            "10.0.0.2".parse().unwrap(),
            &forwarding(&rules, false),
        );
        assert_eq!(headers[header::HOST], "example.com:8443");
        assert_eq!(headers[header::VIA], "2 edge");
    }

    #[test]
    fn ipv6_nodes_are_bracketed_and_quoted() {
        assert_eq!(quote(&node("[::1]:80".parse().unwrap())), "\"[::1]:80\"");
        assert_eq!(quote("rxh"), "rxh");
    }
}
