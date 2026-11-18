//! Request matchers: which `match` entry handles an HTTP request.
//!
//! All matchers of an entry must match (AND). Inside one list (`host`, `path`,
//! `method`) any item may match (OR). An empty matcher matches everything.

use std::net::IpAddr;
use std::str::FromStr;

use http::{HeaderMap, HeaderName, Method, Uri};

use crate::config::Pattern;
use crate::net::IpNet;

/// A host name pattern: exact (`example.com`) or leading wildcard
/// (`*.example.com`, one or more labels, never the bare domain).
///
/// Matching ignores case and the port of the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPattern {
    /// Lowercase name, or the lowercase suffix including the dot for wildcards.
    name: String,
    wildcard: bool,
}

impl HostPattern {
    /// Parses and validates a host pattern.
    ///
    /// # Errors
    ///
    /// Fails if the pattern is empty or uses `*` anywhere but as a leading `*.`.
    pub fn new(pattern: &str) -> Result<Self, String> {
        let wildcard = pattern.starts_with("*.");
        let body = if wildcard { &pattern[2..] } else { pattern };
        if body.is_empty() || body.contains('*') {
            return Err(format!(
                "invalid host {pattern:?}, expected a name or a leading wildcard like *.example.com"
            ));
        }
        // Wildcards keep the dot: `.example.com`.
        let name = if wildcard { &pattern[1..] } else { pattern };
        Ok(Self {
            name: name.to_ascii_lowercase(),
            wildcard,
        })
    }

    /// Returns true if `host` (optionally with `:port`) matches.
    #[must_use]
    pub fn matches(&self, host: &str) -> bool {
        let host = strip_port(host).to_ascii_lowercase();
        if self.wildcard {
            host.len() > self.name.len() && host.ends_with(&self.name)
        } else {
            host == self.name
        }
    }
}

/// Removes the port of `host:port` and `[::1]:port`.
fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return match host.find(']') {
            Some(end) => &host[..=end],
            None => host,
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

/// A Caddy style path pattern. Without `*` the path must be equal. Each `*`
/// matches any run of characters (including `/`), so `/api/*` is a prefix,
/// `*.php` a suffix and `/user/*/profile` has a wildcard in the middle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPattern {
    parts: Vec<String>,
}

impl PathPattern {
    /// Parses and validates a path pattern.
    ///
    /// # Errors
    ///
    /// Fails unless the pattern starts with `/` or `*`.
    pub fn new(pattern: &str) -> Result<Self, String> {
        if !pattern.starts_with('/') && !pattern.starts_with('*') {
            return Err(format!(
                "invalid path {pattern:?}, it must start with / or *"
            ));
        }
        Ok(Self {
            parts: pattern.split('*').map(str::to_owned).collect(),
        })
    }

    /// Returns true if `path` matches.
    #[must_use]
    pub fn matches(&self, path: &str) -> bool {
        let [first, middle @ .., last] = self.parts.as_slice() else {
            return path == self.parts[0];
        };
        let Some(mut rest) = path.strip_prefix(first.as_str()) else {
            return false;
        };
        for part in middle {
            match rest.find(part.as_str()) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
        rest.ends_with(last.as_str())
    }
}

/// The parts of a request that matchers look at.
#[derive(Debug, Clone, Copy)]
pub struct MatchInput<'a> {
    /// Request method.
    pub method: &'a Method,

    /// Request URI. Its authority is the host for HTTP/2.
    pub uri: &'a Uri,

    /// Request headers.
    pub headers: &'a HeaderMap,

    /// Address of the directly connected peer.
    pub peer: IpAddr,
}

impl MatchInput<'_> {
    /// Host of the request: the `Host` header, else the URI authority.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.headers
            .get(http::header::HOST)
            .and_then(|value| value.to_str().ok())
            .or_else(|| self.uri.authority().map(|authority| authority.as_str()))
    }
}

/// Compiled matchers of one `match` entry.
#[derive(Debug, Clone)]
pub struct RequestMatcher {
    uri: String,
    hosts: Vec<HostPattern>,
    paths: Vec<PathPattern>,
    methods: Vec<Method>,
    headers: Vec<(HeaderName, String)>,
    query: Vec<(String, String)>,
    remote_ip: Vec<IpNet>,
}

impl RequestMatcher {
    /// Compiles the matchers of `pattern`. `server_host` applies when the
    /// pattern has no `host` of its own.
    ///
    /// # Errors
    ///
    /// Fails with a message if a host, path, method or header name is invalid.
    pub fn new(pattern: &Pattern, server_host: &[String]) -> Result<Self, String> {
        let hosts = if pattern.host.is_empty() {
            server_host
        } else {
            &pattern.host
        };
        let methods = pattern
            .method
            .iter()
            .map(|method| {
                Method::from_str(&method.to_ascii_uppercase())
                    .map_err(|_| format!("invalid method {method:?}"))
            })
            .collect::<Result<_, _>>()?;
        let headers = pattern
            .header
            .iter()
            .map(|(name, value)| {
                let name = HeaderName::from_str(name)
                    .map_err(|_| format!("invalid header name {name:?}"))?;
                Ok((name, value.clone()))
            })
            .collect::<Result<_, String>>()?;

        Ok(Self {
            uri: pattern.uri.clone(),
            hosts: hosts
                .iter()
                .map(|host| HostPattern::new(host))
                .collect::<Result<_, _>>()?,
            paths: pattern
                .path
                .iter()
                .map(|path| PathPattern::new(path))
                .collect::<Result<_, _>>()?,
            methods,
            headers,
            query: pattern
                .query
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            remote_ip: pattern.remote_ip.clone(),
        })
    }

    /// Returns true if every configured matcher accepts the request.
    #[must_use]
    pub fn matches(&self, request: &MatchInput<'_>) -> bool {
        let path = request.uri.path();
        path.starts_with(self.uri.as_str())
            && (self.hosts.is_empty()
                || request
                    .host()
                    .is_some_and(|host| self.hosts.iter().any(|pattern| pattern.matches(host))))
            && (self.paths.is_empty() || self.paths.iter().any(|pattern| pattern.matches(path)))
            && (self.methods.is_empty() || self.methods.contains(request.method))
            && self.headers.iter().all(|(name, want)| {
                let mut values = request.headers.get_all(name).iter();
                if want == "*" {
                    values.next().is_some()
                } else {
                    values.any(|value| value.as_bytes() == want.as_bytes())
                }
            })
            && self.query.iter().all(|(key, want)| {
                // ponytail: no percent decoding of query keys and values.
                let mut pairs = request
                    .uri
                    .query()
                    .unwrap_or("")
                    .split('&')
                    .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
                    .filter(|(name, _)| name == key);
                if want == "*" {
                    pairs.next().is_some()
                } else {
                    pairs.any(|(_, value)| value == want)
                }
            })
            && (self.remote_ip.is_empty()
                || self.remote_ip.iter().any(|net| net.contains(request.peer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(toml: &str) -> RequestMatcher {
        let text = format!("[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"a:1\"\n{toml}");
        let config = crate::config::Config::from_toml(&text).expect("valid config");
        let server = &config.servers[0];
        RequestMatcher::new(&server.patterns[0], &server.host).expect("valid matcher")
    }

    fn check(matcher: &RequestMatcher, method: &str, uri: &str, headers: &[(&str, &str)]) -> bool {
        check_peer(matcher, method, uri, headers, "10.1.2.3")
    }

    fn check_peer(
        matcher: &RequestMatcher,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        peer: &str,
    ) -> bool {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_str(name).expect("name"),
                value.parse().expect("value"),
            );
        }
        matcher.matches(&MatchInput {
            method: &Method::from_str(method).expect("method"),
            uri: &uri.parse().expect("uri"),
            headers: &map,
            peer: peer.parse().expect("ip"),
        })
    }

    #[test]
    fn host_patterns() {
        let exact = HostPattern::new("Example.com").expect("valid");
        assert!(exact.matches("example.COM"));
        assert!(exact.matches("example.com:8080"));
        assert!(!exact.matches("www.example.com"));

        let wild = HostPattern::new("*.example.com").expect("valid");
        assert!(wild.matches("a.example.com"));
        assert!(wild.matches("A.B.example.com:443"));
        assert!(!wild.matches("example.com"));
        assert!(!wild.matches("badexample.com"));

        assert!(
            HostPattern::new("[::1]")
                .expect("valid")
                .matches("[::1]:80")
        );
        for bad in ["", "*", "*.", "a.*.com", "*example.com"] {
            assert!(HostPattern::new(bad).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn path_patterns() {
        let cases = [
            ("/health", "/health", true),
            ("/health", "/health/", false),
            ("/api/*", "/api/x/y", true),
            ("/api/*", "/api/", true),
            ("/api/*", "/api", false),
            ("*.php", "/a/index.php", true),
            ("*.php", "/a/index.phps", false),
            ("/user/*/profile", "/user/42/profile", true),
            ("/user/*/profile", "/user/42/settings", false),
            ("/a/*/b/*/c", "/a/1/b/2/c", true),
        ];
        for (pattern, path, expected) in cases {
            let pattern_ = PathPattern::new(pattern).expect("valid");
            assert_eq!(pattern_.matches(path), expected, "{pattern} vs {path}");
        }
        assert!(PathPattern::new("api").is_err());
    }

    #[test]
    fn method_matches_ignoring_case() {
        let m = matcher("method = [\"get\", \"Post\"]");
        assert!(check(&m, "GET", "/", &[]));
        assert!(check(&m, "POST", "/", &[]));
        assert!(!check(&m, "PUT", "/", &[]));
    }

    #[test]
    fn header_matches_value_or_presence() {
        let m = matcher("header = { x-env = \"prod\", x-any = \"*\" }");
        assert!(check(&m, "GET", "/", &[("x-env", "prod"), ("X-Any", "1")]));
        assert!(check(
            &m,
            "GET",
            "/",
            &[("x-env", "dev"), ("x-env", "prod"), ("x-any", "")]
        ));
        assert!(!check(&m, "GET", "/", &[("x-env", "prod")]));
        assert!(!check(&m, "GET", "/", &[("x-env", "dev"), ("x-any", "1")]));
    }

    #[test]
    fn query_matches_value_or_presence() {
        let m = matcher("query = { v = \"2\", debug = \"*\" }");
        assert!(check(&m, "GET", "/?v=2&debug", &[]));
        assert!(check(&m, "GET", "/p?debug=0&v=1&v=2", &[]));
        assert!(!check(&m, "GET", "/?v=1&debug", &[]));
        assert!(!check(&m, "GET", "/", &[]));
    }

    #[test]
    fn remote_ip_matches_peer_network() {
        let m = matcher("remote_ip = [\"10.0.0.0/8\", \"::1\"]");
        assert!(check_peer(&m, "GET", "/", &[], "10.9.9.9"));
        assert!(check_peer(&m, "GET", "/", &[], "::1"));
        assert!(!check_peer(&m, "GET", "/", &[], "192.168.1.1"));
    }

    #[test]
    fn host_header_or_authority_without_port() {
        let m = matcher("host = \"a.test\"");
        assert!(check(&m, "GET", "/", &[("host", "A.test:8080")]));
        assert!(check(&m, "GET", "http://a.test/", &[]));
        assert!(!check(&m, "GET", "/", &[("host", "b.test")]));
        assert!(!check(&m, "GET", "/", &[]));
    }

    #[test]
    fn matchers_are_anded_with_uri() {
        let m = matcher("host = \"a.test\"\npath = \"/x/*\"\nmethod = \"GET\"");
        assert!(check(&m, "GET", "/x/1", &[("host", "a.test")]));
        assert!(!check(&m, "POST", "/x/1", &[("host", "a.test")]));
        assert!(!check(&m, "GET", "/y", &[("host", "a.test")]));
    }

    #[test]
    fn server_host_is_the_default() {
        let config = crate::config::Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:1\"\nhost = \"s.test\"\n\
             [[server.match]]\nforward = \"a:1\"\n\
             [[server.match]]\nhost = \"o.test\"\nforward = \"a:1\"\n",
        )
        .expect("valid");
        let server = &config.servers[0];
        let first = RequestMatcher::new(&server.patterns[0], &server.host).expect("ok");
        let second = RequestMatcher::new(&server.patterns[1], &server.host).expect("ok");
        assert!(check(&first, "GET", "/", &[("host", "s.test")]));
        assert!(!check(&first, "GET", "/", &[("host", "o.test")]));
        assert!(check(&second, "GET", "/", &[("host", "o.test")]));
    }

    #[test]
    fn config_rejects_bad_matchers() {
        for extra in [
            "path = \"api\"",
            "host = \"a.*.com\"",
            "method = \"GE T\"",
            "header = { \"bad name\" = \"x\" }",
            "colour = \"red\"",
        ] {
            let text = format!("[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"a:1\"\n{extra}");
            assert!(crate::config::Config::from_toml(&text).is_err(), "{extra}");
            let text = format!(
                "[[server]]\nlisten = \"127.0.0.1:1\"\n[[server.match]]\nforward = \"a:1\"\n{extra}"
            );
            assert!(crate::config::Config::from_toml(&text).is_err(), "{extra}");
        }
    }
}
