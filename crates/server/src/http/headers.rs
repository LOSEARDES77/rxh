//! Header rewriting between client and upstream.
//!
//! Requests get hop-by-hop headers removed (RFC 9110 section 7.6.1), the
//! `X-Forwarded-For`, `X-Forwarded-Proto` and `X-Forwarded-Host` headers set
//! like Caddy does, an RFC 7239 `Forwarded` header and a `Via` entry. Client
//! supplied `X-Forwarded-*` values are only kept when the peer is listed in
//! `trusted_proxies`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};

use hyper::HeaderMap;
use hyper::Method;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::Version;
use hyper::header::{self, HeaderName, HeaderValue};
use regex::Regex;
use rxh_core::config::{HeaderReplace, RequestHeaders, ResponseHeaders};
use rxh_core::net::IpNet;

use crate::http::response;

const X_FORWARDED_FOR: HeaderName = HeaderName::from_static("x-forwarded-for");
const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");
const X_FORWARDED_METHOD: HeaderName = HeaderName::from_static("x-forwarded-method");
const X_FORWARDED_URI: HeaderName = HeaderName::from_static("x-forwarded-uri");

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

    /// Canonical IP of `peer` as a header value, see [`peer_ip_value`].
    pub peer_ip: &'a HeaderValue,

    /// Local socket that accepted the connection.
    pub local: SocketAddr,

    /// True if `peer` is a trusted proxy.
    pub trusted: bool,

    /// True if the client connection uses TLS.
    pub tls: bool,

    /// Proxy identifier for `Forwarded: by=`.
    pub by: &'a str,

    /// `Via` entries of the proxy.
    pub via: &'a Via,

    /// User header rules.
    pub rules: &'a RequestHeaders,

    /// Compiled `rules.replace`.
    pub replace: &'a [Replacement],
}

/// `X-Forwarded-For` entry of a peer. Built once per client connection.
pub(crate) fn peer_ip_value(peer: SocketAddr) -> HeaderValue {
    HeaderValue::from_str(&peer.ip().to_canonical().to_string())
        .expect("an IP address is visible ASCII")
}

/// Compiled `replace` rule of a header.
#[derive(Debug)]
pub(crate) struct Replacement {
    name: HeaderName,
    search: Regex,
    replace: String,
}

impl Replacement {
    /// Compiles the rules; an invalid regex is an error message.
    pub(crate) fn compile(rules: &HashMap<String, HeaderReplace>) -> Result<Vec<Self>, String> {
        rules
            .iter()
            .map(|(name, rule)| {
                Ok(Replacement {
                    name: HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| format!("invalid header name {name:?}"))?,
                    search: Regex::new(&rule.search)
                        .map_err(|err| format!("replace.{name}: invalid regex: {err}"))?,
                    replace: rule.replace.clone(),
                })
            })
            .collect()
    }
}

/// Applies `remove`, `replace`, `set` then `add`.
fn apply_rules(
    headers: &mut HeaderMap,
    remove: &[String],
    replace: &[Replacement],
    set: &HashMap<String, String>,
    add: &HashMap<String, String>,
) {
    for name in remove {
        match name.strip_suffix('*') {
            Some(prefix) => {
                let prefix = prefix.to_ascii_lowercase();
                let matching: Vec<HeaderName> = headers
                    .keys()
                    .filter(|name| name.as_str().starts_with(&prefix))
                    .cloned()
                    .collect();
                for name in matching {
                    headers.remove(name);
                }
            }
            None => {
                if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
                    headers.remove(name);
                }
            }
        }
    }

    for rule in replace {
        let values: Vec<HeaderValue> = headers.get_all(&rule.name).iter().cloned().collect();
        if values.is_empty() {
            continue;
        }
        headers.remove(&rule.name);
        for value in values {
            let rewritten = value
                .to_str()
                .ok()
                .map(|text| rule.search.replace_all(text, rule.replace.as_str()))
                .and_then(|text| HeaderValue::from_str(&text).ok());
            headers.append(&rule.name, rewritten.unwrap_or(value));
        }
    }

    for (name, value) in set {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.insert(name, value);
        }
    }
    for (name, value) in add {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
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

    let host = ensure_host(headers, uri, forwarding.local);
    let proto = if forwarding.tls { "https" } else { "http" };

    set_x_forwarded(headers, client_ip, forwarding, &host, proto);
    set_forwarded(headers, forwarding, &host, proto);
    forwarding.via.append_to(headers, version);

    let rules = forwarding.rules;
    apply_rules(
        headers,
        &rules.remove,
        forwarding.replace,
        &rules.set,
        &rules.add,
    );
}

/// Sets `Host` from the URI when the request has none, and returns the host
/// the client asked for (`local` when it named none).
fn ensure_host(headers: &mut HeaderMap, uri: &Uri, local: SocketAddr) -> HeaderValue {
    // HTTP/2 clients send `:authority` instead of `Host`.
    if !headers.contains_key(header::HOST)
        && let Some(authority) = uri.authority()
        && let Ok(value) = HeaderValue::from_str(authority.as_str())
    {
        headers.insert(header::HOST, value);
    }

    // A clone shares the bytes of the request head: the host name is not copied.
    match headers.get(header::HOST) {
        Some(value) if value.to_str().is_ok() => value.clone(),
        _ => HeaderValue::from_str(&local.to_string()).expect("a socket address is visible ASCII"),
    }
}

/// Turns the headers of a client request into those of its `forward_auth`
/// check: without the hop-by-hop ones, with `X-Forwarded-For`, `-Proto` and
/// `-Host` as the backend would get them, and with the method and URI of the
/// request in `X-Forwarded-Method` and `X-Forwarded-Uri`.
pub(crate) fn prepare_auth_request(
    headers: &mut HeaderMap,
    method: &Method,
    uri: &Uri,
    client_ip: IpAddr,
    forwarding: &Forwarding<'_>,
) {
    remove_hop_by_hop(headers);
    // The check has no body.
    headers.remove(header::CONTENT_LENGTH);

    let host = ensure_host(headers, uri, forwarding.local);
    let proto = if forwarding.tls { "https" } else { "http" };
    set_x_forwarded(headers, client_ip, forwarding, &host, proto);

    let target = uri.path_and_query().map_or("/", |target| target.as_str());
    // Never the client's own values: they decide what gets authorized.
    headers.remove(&X_FORWARDED_METHOD);
    headers.remove(&X_FORWARDED_URI);
    if let (Ok(method), Ok(target)) = (
        HeaderValue::from_str(method.as_str()),
        HeaderValue::from_str(target),
    ) {
        headers.insert(X_FORWARDED_METHOD, method);
        headers.insert(X_FORWARDED_URI, target);
    }
}

fn set_x_forwarded(
    headers: &mut HeaderMap,
    client_ip: IpAddr,
    forwarding: &Forwarding<'_>,
    host: &HeaderValue,
    proto: &'static str,
) {
    if forwarding.trusted {
        append_list(headers, X_FORWARDED_FOR, forwarding.peer_ip);
    } else if client_ip == forwarding.peer.ip() {
        headers.insert(X_FORWARDED_FOR, forwarding.peer_ip.clone());
    } else {
        set_built(
            headers,
            X_FORWARDED_FOR,
            /* extend= */ false,
            |value| {
                push(value, format_args!("{}", client_ip.to_canonical()));
            },
        );
    }

    if !(forwarding.trusted && headers.contains_key(&X_FORWARDED_PROTO)) {
        headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static(proto));
    }
    if !(forwarding.trusted && headers.contains_key(&X_FORWARDED_HOST)) {
        headers.insert(X_FORWARDED_HOST, host.clone());
    }
}

/// RFC 7239 `Forwarded` header.
fn set_forwarded(
    headers: &mut HeaderMap,
    forwarding: &Forwarding<'_>,
    host: &HeaderValue,
    proto: &str,
) {
    let by = forwarding
        .rules
        .forwarded
        .by
        .as_deref()
        .unwrap_or(forwarding.by);
    let host = host.to_str().expect("prepare_request checked the host");

    set_built(
        headers,
        header::FORWARDED,
        forwarding.rules.forwarded.extend,
        |element| {
            element.push_str("for=");
            push_node(element, forwarding.peer);
            element.push_str(";by=");
            push_quoted(element, by);
            element.push_str(";host=");
            push_quoted(element, host);
            element.push_str(";proto=");
            element.push_str(proto);
        },
    );
}

/// Appends the node identifier of RFC 7239 section 6. It always has a colon,
/// so it is always quoted, and IPv6 goes in brackets.
fn push_node(out: &mut String, address: SocketAddr) {
    match address.ip().to_canonical() {
        IpAddr::V4(ip) => push(out, format_args!("\"{ip}:{}\"", address.port())),
        IpAddr::V6(ip) => push(out, format_args!("\"[{ip}]:{}\"", address.port())),
    }
}

/// Appends `value`, quoted unless it is a plain token (RFC 9110 section 5.6.2).
fn push_quoted(out: &mut String, value: &str) {
    let is_token = !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte));
    if is_token {
        out.push_str(value);
        return;
    }
    out.push('"');
    for character in value.chars() {
        if matches!(character, '\\' | '"') {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
}

fn push(out: &mut String, arguments: fmt::Arguments<'_>) {
    fmt::Write::write_fmt(out, arguments).expect("writing to a String cannot fail");
}

/// Rewrites the headers of an upstream response before it goes to the client.
pub(crate) fn prepare_response<B>(
    response: &mut Response<B>,
    rules: &ResponseHeaders,
    replace: &[Replacement],
    via: &Via,
) {
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        remove_hop_by_hop(response.headers_mut());
    }

    let version = response.version();
    let headers = response.headers_mut();

    if rules.server.should_override {
        headers.insert(
            header::SERVER,
            response::server_header(rules.server.version),
        );
    }
    if rules.via.extend {
        via.append_to(headers, version);
    }
    apply_rules(headers, &rules.remove, replace, &rules.set, &rules.add);
}

/// `Via` entries of a proxy, one per HTTP version. They are built once and
/// shared by every message, which only bumps a reference count.
#[derive(Debug)]
pub(crate) struct Via([Option<HeaderValue>; 5]);

impl Via {
    const VERSIONS: [&'static str; 5] = ["0.9", "1.0", "1.1", "2", "3"];

    /// Entries of the proxy called `by`.
    pub(crate) fn new(by: &str) -> Self {
        Self(Self::VERSIONS.map(|version| HeaderValue::from_str(&format!("{version} {by}")).ok()))
    }

    /// Adds the entry of a message received with `version` to its `Via` list.
    fn append_to(&self, headers: &mut HeaderMap, version: Version) {
        let index = match version {
            Version::HTTP_09 => 0,
            Version::HTTP_10 => 1,
            Version::HTTP_2 => 3,
            Version::HTTP_3 => 4,
            _ => 2,
        };
        // `None` when `by` is not a valid header value: no entry, as before.
        if let Some(entry) = &self.0[index] {
            append_list(headers, header::VIA, entry);
        }
    }
}

thread_local! {
    /// Buffer header values are built in, so that building one does not
    /// allocate once the buffer has grown.
    static SCRATCH: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Sets `name` to the text written by `build`. With `extend` it goes after
/// the values the header already has, as a comma separated list.
fn set_built(
    headers: &mut HeaderMap,
    name: HeaderName,
    extend: bool,
    build: impl FnOnce(&mut String),
) {
    let value: Option<HeaderValue> = SCRATCH.with_borrow_mut(|scratch| {
        scratch.clear();
        if extend {
            for previous in headers.get_all(&name) {
                if let Ok(previous) = previous.to_str() {
                    scratch.push_str(previous.trim());
                    scratch.push_str(", ");
                }
            }
        }
        build(scratch);
        HeaderValue::from_str(scratch).ok()
    });
    if let Some(value) = value {
        headers.insert(name, value);
    }
}

/// Appends `value` to a comma separated list header.
fn append_list(headers: &mut HeaderMap, name: HeaderName, value: &HeaderValue) {
    if !headers.contains_key(&name) {
        headers.insert(name, value.clone());
        return;
    }
    if let Ok(text) = value.to_str() {
        set_built(headers, name, /* extend= */ true, |list| {
            list.push_str(text);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarding(rules: &RequestHeaders, trusted: bool) -> Forwarding<'_> {
        let peer: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        Forwarding {
            peer,
            peer_ip: Box::leak(Box::new(peer_ip_value(peer))),
            local: "127.0.0.1:8080".parse().unwrap(),
            trusted,
            tls: false,
            by: "edge",
            via: Box::leak(Box::new(Via::new("edge"))),
            rules,
            replace: &[],
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
    fn rules_apply_in_order_with_wildcards_and_regex() {
        let mut rules = RequestHeaders {
            remove: vec!["x-debug-*".into()],
            ..RequestHeaders::default()
        };
        rules.replace.insert(
            "x-path".into(),
            HeaderReplace {
                search: "^/v1/(.*)$".into(),
                replace: "/v2/$1".into(),
            },
        );
        rules.set.insert("x-set".into(), "set".into());
        rules.add.insert("x-multi".into(), "two".into());
        let replace = Replacement::compile(&rules.replace).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-debug-a", "1".parse().unwrap());
        headers.insert("x-debug-b", "1".parse().unwrap());
        headers.insert("x-keep", "1".parse().unwrap());
        headers.insert("x-path", "/v1/users".parse().unwrap());
        headers.append("x-multi", "one".parse().unwrap());
        apply_rules(
            &mut headers,
            &rules.remove,
            &replace,
            &rules.set,
            &rules.add,
        );

        assert!(!headers.contains_key("x-debug-a") && !headers.contains_key("x-debug-b"));
        assert_eq!(headers["x-keep"], "1");
        assert_eq!(headers["x-path"], "/v2/users");
        assert_eq!(headers["x-set"], "set");
        let multi: Vec<_> = headers.get_all("x-multi").iter().collect();
        assert_eq!(multi, ["one", "two"]);
    }

    #[test]
    fn invalid_regex_is_an_error() {
        let rules = HashMap::from([(
            "x".to_owned(),
            HeaderReplace {
                search: "(".into(),
                replace: String::new(),
            },
        )]);
        assert!(Replacement::compile(&rules).is_err());
    }

    #[test]
    fn ipv6_nodes_are_bracketed_and_quoted() {
        let mut node = String::new();
        push_node(&mut node, "[::1]:80".parse().unwrap());
        assert_eq!(node, "\"[::1]:80\"");
        node.clear();
        push_node(&mut node, "[::ffff:10.0.0.2]:80".parse().unwrap());
        assert_eq!(node, "\"10.0.0.2:80\"");
    }

    #[test]
    fn only_values_that_are_not_tokens_are_quoted_and_escaped() {
        let quoted = |value: &str| {
            let mut out = String::new();
            push_quoted(&mut out, value);
            out
        };
        assert_eq!(quoted("rxh"), "rxh");
        assert_eq!(quoted(""), "\"\"");
        assert_eq!(quoted("example.com:8080"), "\"example.com:8080\"");
        assert_eq!(quoted(r#"a"b\c"#), r#""a\"b\\c""#);
    }

    #[test]
    fn lists_are_extended_and_forwarded_is_replaced_unless_extend() {
        let mut rules = RequestHeaders::default();
        rules.forwarded.extend = false;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "example.com".parse().unwrap());
        headers.append(header::VIA, "1.0 first".parse().unwrap());
        headers.append(header::VIA, " 1.1 second ".parse().unwrap());
        headers.insert(header::FORWARDED, "for=1.2.3.4".parse().unwrap());

        let mut replaced = headers.clone();
        prepare_request(
            &mut replaced,
            Version::HTTP_11,
            &Uri::from_static("/"),
            "10.0.0.2".parse().unwrap(),
            &forwarding(&rules, false),
        );
        assert_eq!(replaced[header::VIA], "1.0 first, 1.1 second, 1.1 edge");
        assert_eq!(replaced.get_all(header::VIA).iter().count(), 1);
        assert_eq!(
            replaced[header::FORWARDED],
            "for=\"10.0.0.2:5000\";by=edge;host=example.com;proto=http"
        );

        rules.forwarded.extend = true;
        rules.forwarded.by = Some("my proxy".into());
        prepare_request(
            &mut headers,
            Version::HTTP_11,
            &Uri::from_static("/"),
            "10.0.0.2".parse().unwrap(),
            &forwarding(&rules, false),
        );
        assert_eq!(
            headers[header::FORWARDED],
            "for=1.2.3.4, for=\"10.0.0.2:5000\";by=\"my proxy\";host=example.com;proto=http"
        );
    }

    #[test]
    fn missing_host_falls_back_to_the_local_address() {
        let rules = RequestHeaders::default();
        let mut headers = HeaderMap::new();
        prepare_request(
            &mut headers,
            Version::HTTP_10,
            &Uri::from_static("/"),
            "10.0.0.2".parse().unwrap(),
            &forwarding(&rules, false),
        );
        assert_eq!(headers[&X_FORWARDED_HOST], "127.0.0.1:8080");
        assert_eq!(headers[header::VIA], "1.0 edge");
    }

    #[test]
    fn response_gets_server_and_via() {
        let mut response = Response::new(());
        response
            .headers_mut()
            .insert(header::SERVER, "backend".parse().unwrap());
        let mut rules = ResponseHeaders::default();
        rules.via.extend = true;
        prepare_response(&mut response, &rules, &[], &Via::new("edge"));
        let server = format!("rxh/{}", crate::VERSION);
        assert_eq!(response.headers()[header::SERVER], server.as_str());
        assert_eq!(response.headers()[header::VIA], "1.1 edge");
    }
}
