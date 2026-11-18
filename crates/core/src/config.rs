//! Configuration file model.
//!
//! The whole file deserializes into [`Config`]. A minimal file looks like this:
//!
//! ```toml
//! [upstream.api]
//! policy = "least_conn"
//! backends = ["127.0.0.1:8080", "127.0.0.1:8081"]
//!
//! [[server]]
//! listen = "0.0.0.0:80"
//! forward = "@api"
//!
//! [[server]]
//! listen = "0.0.0.0:5432"
//! protocol = "tcp"
//! forward = ["10.0.0.1:5432", "10.0.0.2:5432"]
//! ```
//!
//! [`Config::from_toml`] parses and validates a file in one step.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize, Serializer};

use crate::address::{self, BackendAddress, Scheme};
use crate::duration;
use crate::matcher::RequestMatcher;
use crate::net::IpNet;
use crate::status::StatusMatcher;

/// Error produced while loading a configuration file.
#[derive(Debug)]
pub enum ConfigError {
    /// The file is not valid TOML or does not match the schema.
    Toml(toml::de::Error),

    /// The file parses but describes an impossible setup.
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Toml(err) => write!(f, "{err}"),
            ConfigError::Invalid(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<toml::de::Error> for ConfigError {
    fn from(value: toml::de::Error) -> Self {
        ConfigError::Toml(value)
    }
}

/// Entire configuration file.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Admin API settings.
    #[serde(default)]
    pub admin: Admin,

    /// Maximum time to wait for open connections during shutdown or reload.
    /// `None` waits forever, like Caddy's default.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub grace_period: Option<Duration>,

    /// Named load balancing groups, referenced from servers as `"@name"`.
    #[serde(
        default,
        rename = "upstream",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub upstreams: BTreeMap<String, UpstreamGroup>,

    /// Listeners.
    #[serde(default, rename = "server")]
    pub servers: Vec<Server>,
}

impl Config {
    /// Parses and validates a TOML document.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Checks references and protocol constraints that serde cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: String| Err(ConfigError::Invalid(message));

        for (name, group) in &self.upstreams {
            group
                .validate()
                .map_err(|err| ConfigError::Invalid(format!("upstream.{name}: {err}")))?;
        }

        // Servers sharing a listen address, keyed by address and "is UDP".
        let mut listeners: HashMap<(SocketAddr, bool), (String, &Server)> = HashMap::new();

        for (index, server) in self.servers.iter().enumerate() {
            let label = server.label(index);

            // Port 0 always gets its own socket, it is never shared.
            for &address in server.listen.iter().filter(|address| address.port() != 0) {
                let key = (address, server.protocol == Protocol::Udp);
                let Some((first_label, first)) = listeners.get(&key) else {
                    listeners.insert(key, (label.clone(), server));
                    continue;
                };
                if server.protocol != Protocol::Http || first.protocol != Protocol::Http {
                    return invalid(format!(
                        "{label}: listen address {address} is already used by {first_label}, \
                         only http servers can share a listen address"
                    ));
                }
                if server.tls.is_some() != first.tls.is_some() {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         either all or none of the servers on it must use TLS"
                    ));
                }
            }

            for (pattern_index, pattern) in server.patterns.iter().enumerate() {
                RequestMatcher::new(pattern, &server.host).map_err(|err| {
                    ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                })?;
                let Action::Forward(forward) = &pattern.action else {
                    continue;
                };

                let group = match forward {
                    Forward::Group(name) => match self.upstreams.get(name) {
                        Some(group) => group,
                        None => return invalid(format!("{label}: unknown upstream group @{name}")),
                    },
                    Forward::Inline(group) => {
                        group.validate().map_err(|err| {
                            ConfigError::Invalid(format!("{label}: forward: {err}"))
                        })?;
                        if group.backends.is_empty() && group.dynamic.is_none() {
                            return invalid(format!("{label}: forward needs at least one backend"));
                        }
                        group
                    }
                };

                if server.protocol != Protocol::Http && group.policy.requires_http() {
                    return invalid(format!(
                        "{label}: policy {} needs HTTP requests, it cannot be used with {}",
                        group.policy, server.protocol
                    ));
                }
            }

            if server.protocol != Protocol::Http {
                let [pattern] = server.patterns.as_slice() else {
                    return invalid(format!(
                        "{label}: {} servers take a single forward",
                        server.protocol
                    ));
                };
                if !matches!(pattern.action, Action::Forward(_)) {
                    return invalid(format!(
                        "{label}: {} servers can only forward",
                        server.protocol
                    ));
                }
            }

            if server.protocol == Protocol::Udp
                && let Some(Action::Forward(forward)) = server.patterns.first().map(|p| &p.action)
                && let Some(group) = match forward {
                    Forward::Group(name) => self.upstreams.get(name),
                    Forward::Inline(group) => Some(&**group),
                }
                && group
                    .backends
                    .iter()
                    .any(|backend| backend.address.starts_with("unix/"))
            {
                return invalid(format!(
                    "{label}: unix// backends are not supported for udp servers"
                ));
            }

            if server.protocol == Protocol::Udp && server.tls.is_some() {
                return invalid(format!("{label}: TLS is not supported for UDP"));
            }

            if server.proxy_protocol.is_some() && server.protocol != Protocol::Tcp {
                return invalid(format!(
                    "{label}: proxy_protocol only applies to tcp servers"
                ));
            }
        }

        Ok(())
    }
}

/// Admin API settings. The admin API is how the CLI talks to a running daemon.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    /// Disables the admin API when false.
    #[serde(default = "default::true_value")]
    pub enabled: bool,

    /// Address of the admin API. Keep it on loopback, it has no authentication.
    #[serde(default = "default::admin_listen")]
    pub listen: SocketAddr,
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: default::admin_listen(),
        }
    }
}

/// Protocol spoken by a listener.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// HTTP/1.1 and HTTP/2 reverse proxy, file server and cache.
    #[default]
    Http,

    /// Raw TCP (layer 4) proxy. Optional TLS termination.
    Tcp,

    /// Raw UDP (layer 4) proxy with per-client sessions.
    Udp,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Protocol::Http => "http",
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        })
    }
}

/// PROXY protocol header sent to upstreams on new TCP connections.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProxyProtocol {
    /// Human readable version 1 header.
    V1,

    /// Binary version 2 header.
    V2,
}

/// A single `[[server]]` entry. Each listen address becomes its own listener.
///
/// A server can describe one pattern inline (`forward`, `serve`, `uri`,
/// `request`, `response`, `cache`) or many patterns under `match`, not both.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(try_from = "RawServer")]
pub struct Server {
    /// Socket addresses where this server listens.
    pub listen: Vec<SocketAddr>,

    /// Protocol spoken on the listening sockets.
    pub protocol: Protocol,

    /// Patterns matched in order against the request path.
    #[serde(rename = "match")]
    pub patterns: Vec<Pattern>,

    /// Default `host` matcher of the entries that have none of their own.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host: Vec<String>,

    /// Maximum number of concurrent client connections.
    pub max_connections: usize,

    /// Name shown in logs and in the `Forwarded` and `Via` headers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// TLS termination for incoming connections.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<Tls>,

    /// Accepts HTTP/2 (h2 over TLS and h2c) on this listener.
    pub http2: bool,

    /// Peers allowed to set `X-Forwarded-*` headers and the real client IP.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<IpNet>,

    /// PROXY protocol header to send upstream (tcp only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_protocol: Option<ProxyProtocol>,

    /// Idle time after which a UDP session is dropped.
    #[serde(with = "duration")]
    pub udp_idle_timeout: Duration,
}

impl Server {
    /// Returns a label such as `server[0] (api)` for error messages.
    #[must_use]
    pub fn label(&self, index: usize) -> String {
        match &self.name {
            Some(name) => format!("server[{index}] ({name})"),
            None => format!("server[{index}]"),
        }
    }
}

/// Flat representation of [`Server`] as written in the file.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    #[serde(deserialize_with = "one_or_many")]
    listen: Vec<SocketAddr>,
    #[serde(default)]
    protocol: Protocol,
    #[serde(default, rename = "match")]
    patterns: Option<Vec<Pattern>>,
    forward: Option<Forward>,
    serve: Option<String>,
    uri: Option<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    host: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    path: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    method: Vec<String>,
    #[serde(default)]
    header: BTreeMap<String, String>,
    #[serde(default)]
    query: BTreeMap<String, String>,
    #[serde(default)]
    remote_ip: Vec<IpNet>,
    request: Option<RequestConfig>,
    response: Option<ResponseConfig>,
    cache: Option<Cache>,
    #[serde(default = "default::max_connections", alias = "connections")]
    max_connections: usize,
    name: Option<String>,
    tls: Option<Tls>,
    #[serde(default = "default::true_value")]
    http2: bool,
    #[serde(default)]
    trusted_proxies: Vec<String>,
    proxy_protocol: Option<ProxyProtocol>,
    #[serde(default = "default::udp_idle_timeout", with = "duration")]
    udp_idle_timeout: Duration,
}

impl TryFrom<RawServer> for Server {
    type Error = String;

    fn try_from(raw: RawServer) -> Result<Self, Self::Error> {
        if raw.listen.is_empty() {
            return Err(String::from("missing field `listen`"));
        }

        let inline_action = match (raw.forward, raw.serve) {
            (Some(_), Some(_)) => {
                return Err(String::from(
                    "use either 'forward' or 'serve', if you need multiple patterns use 'match'",
                ));
            }
            (Some(forward), None) => Some(Action::Forward(forward)),
            (None, Some(root)) => Some(Action::Serve(root)),
            (None, None) => None,
        };

        let has_inline_options = raw.uri.is_some()
            || !raw.path.is_empty()
            || !raw.method.is_empty()
            || !raw.header.is_empty()
            || !raw.query.is_empty()
            || !raw.remote_ip.is_empty()
            || raw.request.is_some()
            || raw.response.is_some()
            || raw.cache.is_some();

        let patterns = match (raw.patterns, inline_action) {
            (Some(_), Some(_)) => {
                return Err(String::from(
                    "either use 'match' for multiple patterns or describe a single pattern",
                ));
            }
            (Some(_), None) if has_inline_options => {
                return Err(String::from(
                    "'uri', 'path', 'method', 'header', 'query', 'remote_ip', 'request', \
                     'response' and 'cache' go inside each 'match' entry",
                ));
            }
            (Some(patterns), None) if !patterns.is_empty() => patterns,
            (None, Some(action)) => vec![Pattern {
                uri: raw.uri.unwrap_or_else(default::uri),
                host: Vec::new(),
                path: raw.path,
                method: raw.method,
                header: raw.header,
                query: raw.query,
                remote_ip: raw.remote_ip,
                action,
                request: raw.request.unwrap_or_default(),
                response: raw.response.unwrap_or_default(),
                cache: raw.cache.unwrap_or_default(),
            }],
            _ => return Err(String::from("missing 'match', 'forward' or 'serve'")),
        };

        let mut trusted_proxies = Vec::new();
        for range in &raw.trusted_proxies {
            if range == "private_ranges" {
                trusted_proxies.extend(IpNet::private_ranges());
            } else {
                trusted_proxies.push(range.parse::<IpNet>()?);
            }
        }

        Ok(Server {
            listen: raw.listen,
            protocol: raw.protocol,
            patterns,
            host: raw.host,
            max_connections: raw.max_connections,
            name: raw.name,
            tls: raw.tls,
            http2: raw.http2,
            trusted_proxies,
            proxy_protocol: raw.proxy_protocol,
            udp_idle_timeout: raw.udp_idle_timeout,
        })
    }
}

/// One entry of a server's `match` list.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    /// Path prefix to match against.
    #[serde(default = "default::uri")]
    pub uri: String,

    /// Host names, exact or `*.example.com`. Empty means any host (or the
    /// server's `host`).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub host: Vec<String>,

    /// Path patterns: `/exact`, `/prefix/*`, `*.suffix`, `/a/*/b`.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub path: Vec<String>,

    /// HTTP methods, case insensitive.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub method: Vec<String>,

    /// Required request headers. The value `*` only requires presence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,

    /// Required query parameters. The value `*` only requires presence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,

    /// Client networks allowed to match (the directly connected peer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remote_ip: Vec<IpNet>,

    /// What to do when the pattern matches.
    #[serde(flatten)]
    pub action: Action,

    /// Request header customization.
    #[serde(default)]
    pub request: RequestConfig,

    /// Response header customization.
    #[serde(default)]
    pub response: ResponseConfig,

    /// Response cache for this pattern.
    #[serde(default)]
    pub cache: Cache,
}

/// Action executed when a [`Pattern`] matches.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Proxy to a load balancing group.
    Forward(Forward),

    /// Serve static files from a root directory.
    Serve(String),
}

/// Target of a `forward` action: a named group (`"@api"`) or an inline one.
///
/// ```toml
/// forward = "@api"                                  # named group
/// forward = "127.0.0.1:8080"                        # single backend
/// forward = ["127.0.0.1:8080", "127.0.0.1:8081"]    # list of backends
/// forward = { policy = "ip_hash", backends = ["127.0.0.1:8080"] }
/// ```
#[derive(Deserialize, Debug, Clone)]
#[serde(from = "ForwardOption")]
pub enum Forward {
    /// Reference to an entry of [`Config::upstreams`].
    Group(String),

    /// Anonymous group declared in place.
    Inline(Box<UpstreamGroup>),
}

impl Serialize for Forward {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Forward::Group(name) => serializer.serialize_str(&format!("@{name}")),
            Forward::Inline(group) => group.serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ForwardOption {
    Text(String),
    List(Vec<Backend>),
    Group(Box<UpstreamGroup>),
}

impl From<ForwardOption> for Forward {
    fn from(value: ForwardOption) -> Self {
        match value {
            ForwardOption::Text(text) => match text.strip_prefix('@') {
                Some(name) => Forward::Group(name.to_owned()),
                None => {
                    Forward::Inline(Box::new(UpstreamGroup::with_backends(vec![Backend::new(
                        text,
                    )])))
                }
            },
            ForwardOption::List(backends) => {
                Forward::Inline(Box::new(UpstreamGroup::with_backends(backends)))
            }
            ForwardOption::Group(group) => Forward::Inline(group),
        }
    }
}

/// Load balancing group: backends plus selection, retry and health settings.
/// Field names follow Caddy's `reverse_proxy` subdirectives.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct UpstreamGroup {
    /// Selection policy (Caddy `lb_policy`).
    #[serde(default, alias = "lb_policy", alias = "algorithm")]
    pub policy: Policy,

    /// Candidates sampled by `random_choose`.
    #[serde(default = "default::choose")]
    pub choose: usize,

    /// Query parameter, header or cookie name for `query`, `header` and `cookie`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// HMAC secret for the `cookie` policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,

    /// Policy used when the key of a hash policy is missing. Defaults to random.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Policy>,

    /// Extra attempts after a failed connection (Caddy `lb_retries`).
    #[serde(default, alias = "lb_retries")]
    pub retries: u32,

    /// Keep retrying for this long (Caddy `lb_try_duration`).
    #[serde(
        default,
        alias = "lb_try_duration",
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub try_duration: Option<Duration>,

    /// Wait between attempts (Caddy `lb_try_interval`).
    #[serde(
        default = "default::try_interval",
        alias = "lb_try_interval",
        with = "duration"
    )]
    pub try_interval: Duration,

    /// Upstream connect timeout.
    #[serde(default = "default::dial_timeout", with = "duration")]
    pub dial_timeout: Duration,

    /// Speak cleartext HTTP/2 to the backends instead of HTTP/1.1.
    #[serde(default)]
    pub h2c: bool,

    /// How long idle upstream HTTP connections stay in the pool.
    #[serde(default = "default::keepalive", with = "duration")]
    pub keepalive: Duration,

    /// Maximum idle upstream HTTP connections kept per backend.
    #[serde(default = "default::keepalive_idle_conns_per_host")]
    pub keepalive_idle_conns_per_host: usize,

    /// Backends. Port ranges such as `10.0.0.1:8001-8004` expand to many.
    #[serde(default, alias = "to")]
    pub backends: Vec<Backend>,

    /// Active health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<ActiveHealth>,

    /// Passive health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passive: Option<PassiveHealth>,

    /// TLS settings for connections to the backends. Setting this table
    /// enables TLS even without `https://` backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<UpstreamTls>,

    /// Members discovered through DNS, on top of `backends`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynamicUpstreams>,
}

/// IP versions used by [`DynamicUpstreams::a`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IpVersion {
    /// `A` records.
    Ipv4,

    /// `AAAA` records.
    Ipv6,
}

/// DNS driven members of an [`UpstreamGroup`] (Caddy `dynamic a` and `dynamic srv`).
///
/// `a` resolves to plain `ip:port` backends. When the group uses TLS the SNI
/// and certificate name cannot come from an IP, so `tls.server_name` is
/// required. `srv` produces `target:port` backends that keep the host name.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DynamicUpstreams {
    /// `name:port`, resolved to its `A` and `AAAA` records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a: Option<String>,

    /// `_service._proto.name`, resolved to its `SRV` records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub srv: Option<String>,

    /// Time between lookups.
    #[serde(default = "default::dynamic_refresh", with = "duration")]
    pub refresh: Duration,

    /// DNS servers to ask, default the system configuration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolvers: Vec<SocketAddr>,

    /// Record types used with `a`, both by default. Not allowed with `srv`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<Vec<IpVersion>>,
}

impl DynamicUpstreams {
    fn validate(&self) -> Result<(), String> {
        match (&self.a, &self.srv) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(String::from("dynamic needs exactly one of 'a' and 'srv'"));
            }
            (Some(a), None) => match BackendAddress::parse(a) {
                Ok(BackendAddress::Tcp {
                    scheme: Scheme::Bare,
                    ..
                }) => {}
                _ => return Err(format!("dynamic.a must be name:port, got {a:?}")),
            },
            (None, Some(srv)) => {
                if srv.is_empty() || srv.contains(':') || srv.contains('/') {
                    return Err(format!(
                        "dynamic.srv must be _service._proto.name, got {srv:?}"
                    ));
                }
            }
        }
        if self.refresh.is_zero() {
            return Err(String::from("dynamic.refresh must be greater than zero"));
        }
        match &self.versions {
            Some(_) if self.srv.is_some() => {
                return Err(String::from("dynamic.versions only applies to 'a'"));
            }
            Some(versions) if versions.is_empty() => {
                return Err(String::from("dynamic.versions must not be empty"));
            }
            _ => {}
        }
        Ok(())
    }
}

/// TLS client settings of an [`UpstreamGroup`] (Caddy `transport http { tls }`).
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamTls {
    /// Server name sent as SNI and checked against the certificate. Defaults
    /// to the backend host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,

    /// Accept any certificate. Insecure, for tests only.
    #[serde(default)]
    pub insecure_skip_verify: bool,

    /// Path to a PEM file with extra trusted root certificates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,

    /// Path to the PEM client certificate chain (mutual TLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<String>,

    /// Path to the PEM client private key (mutual TLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<String>,
}

impl Default for UpstreamGroup {
    fn default() -> Self {
        Self::with_backends(Vec::new())
    }
}

impl UpstreamGroup {
    /// Group with default settings and the given backends.
    #[must_use]
    pub fn with_backends(backends: Vec<Backend>) -> Self {
        Self {
            policy: Policy::default(),
            choose: default::choose(),
            key: None,
            secret: None,
            fallback: None,
            retries: 0,
            try_duration: None,
            try_interval: default::try_interval(),
            dial_timeout: default::dial_timeout(),
            h2c: false,
            keepalive: default::keepalive(),
            keepalive_idle_conns_per_host: default::keepalive_idle_conns_per_host(),
            backends,
            health: None,
            passive: None,
            tls: None,
            dynamic: None,
        }
    }

    /// True when connections to the backends use TLS: an `https://` backend
    /// or a `tls` table.
    #[must_use]
    pub fn uses_tls(&self) -> bool {
        self.tls.is_some()
            || self
                .backends
                .iter()
                .any(|backend| backend.address.starts_with("https://"))
    }

    /// Backends with port ranges expanded.
    pub fn expanded_backends(&self) -> Result<Vec<Backend>, String> {
        let mut expanded = Vec::new();
        for backend in &self.backends {
            for address in expand_port_range(&backend.address)? {
                expanded.push(Backend {
                    address,
                    weight: backend.weight,
                });
            }
        }
        Ok(expanded)
    }

    fn validate(&self) -> Result<(), String> {
        let backends = self.expanded_backends()?;

        let schemes = |wanted: bool| {
            backends.iter().any(|backend| {
                matches!(
                    BackendAddress::parse(&backend.address),
                    Ok(BackendAddress::Tcp { scheme, .. }) if (scheme == Scheme::Https) == wanted
                ) || (!wanted && backend.address.starts_with("unix/"))
            })
        };
        if schemes(true) && schemes(false) {
            return Err(String::from(
                "schemes can't be mixed: https:// backends cannot share a pool with plain ones",
            ));
        }

        if let Some(tls) = &self.tls
            && tls.client_cert.is_some() != tls.client_key.is_some()
        {
            return Err(String::from(
                "tls.client_cert and tls.client_key must be set together",
            ));
        }

        if let Some(dynamic) = &self.dynamic {
            dynamic.validate()?;
            let needs_name = dynamic.a.is_some() && self.uses_tls();
            if needs_name
                && self
                    .tls
                    .as_ref()
                    .is_none_or(|tls| tls.server_name.is_none())
            {
                return Err(String::from(
                    "dynamic upstreams with TLS need tls.server_name, resolved addresses are IPs",
                ));
            }
        }

        if self.choose == 0 {
            return Err(String::from("choose must be at least 1"));
        }

        if matches!(self.policy, Policy::Query | Policy::Header) && self.key.is_none() {
            return Err(format!("policy {} needs a 'key'", self.policy));
        }

        if self
            .fallback
            .is_some_and(|fallback| fallback.requires_key())
        {
            return Err(String::from(
                "fallback must not be a hash policy that needs a key",
            ));
        }

        if let Some(passive) = &self.passive
            && passive.max_fails == 0
        {
            return Err(String::from("passive.max_fails must be at least 1"));
        }

        if let Some(health) = &self.health
            && (health.passes == 0 || health.fails == 0)
        {
            return Err(String::from(
                "health.passes and health.fails must be at least 1",
            ));
        }

        Ok(())
    }
}

/// Splits `host:port` and checks the port, expanding `host:8001-8004`.
fn expand_port_range(address: &str) -> Result<Vec<String>, String> {
    if address.starts_with("unix/") {
        BackendAddress::parse(address)?;
        return Ok(vec![address.to_owned()]);
    }

    let (_, rest) = address::split_scheme(address);
    let prefix = &address[..address.len() - rest.len()];
    let range = address::split_host_port(rest)
        .and_then(|(host, port)| Some((host, port?.split_once('-')?)));
    let Some((host, (start, end))) = range else {
        BackendAddress::parse(address)?;
        return Ok(vec![address.to_owned()]);
    };

    let invalid = || format!("invalid port range in backend address {address:?}");
    let start: u16 = start.parse().map_err(|_| invalid())?;
    let end: u16 = end.parse().map_err(|_| invalid())?;
    if start > end {
        return Err(invalid());
    }
    Ok((start..=end)
        .map(|port| format!("{prefix}{host}:{port}"))
        .collect())
}

/// One backend of an [`UpstreamGroup`].
///
/// Written as `"host:port"` or `{ address = "host:port", weight = 3 }`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(from = "BackendOption")]
pub struct Backend {
    /// `host:port` of the backend. Host names are resolved on every dial.
    pub address: String,

    /// Relative weight for `weighted_round_robin`. Zero disables new picks.
    pub weight: u32,
}

impl Backend {
    /// Backend with weight 1.
    #[must_use]
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            weight: 1,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BackendOption {
    Simple(String),
    Weighted {
        address: String,
        #[serde(default = "default::weight")]
        weight: u32,
    },
}

impl From<BackendOption> for Backend {
    fn from(value: BackendOption) -> Self {
        match value {
            BackendOption::Simple(address) => Backend::new(address),
            BackendOption::Weighted { address, weight } => Backend { address, weight },
        }
    }
}

/// Load balancing policy. Names match Caddy's `lb_policy` values.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Uniformly random available backend.
    Random,

    /// Samples `choose` random backends and takes the least loaded one.
    RandomChoose,

    /// First available backend in declaration order (active/passive failover).
    First,

    /// Rotates through backends, ignoring weights.
    RoundRobin,

    /// Smooth weighted round robin. The default policy.
    #[default]
    #[serde(alias = "WRR", alias = "wrr")]
    WeightedRoundRobin,

    /// Backend with the fewest active connections, ties broken randomly.
    LeastConn,

    /// Rendezvous hash of the remote peer IP.
    IpHash,

    /// Rendezvous hash of the real client IP (honours `trusted_proxies`).
    ClientIpHash,

    /// Rendezvous hash of the request path and query.
    UriHash,

    /// Rendezvous hash of the query parameter named by `key`.
    Query,

    /// Rendezvous hash of the header named by `key`.
    Header,

    /// Sticky sessions through an HMAC signed cookie.
    Cookie,
}

impl Policy {
    /// Every policy, in documentation order.
    pub const ALL: [Policy; 12] = [
        Policy::Random,
        Policy::RandomChoose,
        Policy::First,
        Policy::RoundRobin,
        Policy::WeightedRoundRobin,
        Policy::LeastConn,
        Policy::IpHash,
        Policy::ClientIpHash,
        Policy::UriHash,
        Policy::Query,
        Policy::Header,
        Policy::Cookie,
    ];

    /// Name used in the configuration file.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Policy::Random => "random",
            Policy::RandomChoose => "random_choose",
            Policy::First => "first",
            Policy::RoundRobin => "round_robin",
            Policy::WeightedRoundRobin => "weighted_round_robin",
            Policy::LeastConn => "least_conn",
            Policy::IpHash => "ip_hash",
            Policy::ClientIpHash => "client_ip_hash",
            Policy::UriHash => "uri_hash",
            Policy::Query => "query",
            Policy::Header => "header",
            Policy::Cookie => "cookie",
        }
    }

    /// True for policies that read HTTP request data.
    #[must_use]
    pub fn requires_http(self) -> bool {
        matches!(
            self,
            Policy::UriHash | Policy::Query | Policy::Header | Policy::Cookie
        )
    }

    /// True for policies that fall back to another policy when data is missing.
    #[must_use]
    pub fn requires_key(self) -> bool {
        matches!(self, Policy::Query | Policy::Header | Policy::Cookie)
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Policy {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let deserializer: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
            text.into_deserializer();
        Policy::deserialize(deserializer).map_err(|_| {
            let names: Vec<&str> = Policy::ALL.iter().map(|policy| policy.name()).collect();
            format!(
                "unknown policy {text:?}, expected one of: {}",
                names.join(", ")
            )
        })
    }
}

/// Active health checks. With `uri` set the check is an HTTP request,
/// otherwise it is a plain TCP connect.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ActiveHealth {
    /// Path and query requested by HTTP checks (Caddy `health_uri`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,

    /// Port to check instead of the backend port (Caddy `health_port`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Fixed `host:port` to check for every backend (Caddy `health_upstream`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,

    /// Time between checks.
    #[serde(default = "default::health_interval", with = "duration")]
    pub interval: Duration,

    /// Time limit for one check.
    #[serde(default = "default::health_timeout", with = "duration")]
    pub timeout: Duration,

    /// Consecutive passes needed to mark a backend healthy.
    #[serde(default = "default::one")]
    pub passes: u32,

    /// Consecutive failures needed to mark a backend unhealthy.
    #[serde(default = "default::one")]
    pub fails: u32,

    /// HTTP method of the check.
    #[serde(default = "default::health_method")]
    pub method: String,

    /// Expected status, `2xx` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusMatcher>,

    /// Regular expression that the response body must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,

    /// Extra request headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    /// Request body sent with the check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,

    /// Follow redirects instead of counting them as failures.
    #[serde(default)]
    pub follow_redirects: bool,
}

/// Passive health checks: backends that fail real traffic are taken out of
/// rotation for `fail_duration`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PassiveHealth {
    /// How long a failure is remembered.
    #[serde(default = "default::fail_duration", with = "duration")]
    pub fail_duration: Duration,

    /// Failures within `fail_duration` that mark a backend down.
    #[serde(default = "default::one")]
    pub max_fails: u32,

    /// Response statuses counted as failures, like `5xx` or `503`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unhealthy_status: Vec<StatusMatcher>,

    /// Responses slower than this count as failures.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub unhealthy_latency: Option<Duration>,

    /// Concurrent requests above which a backend is considered full. Zero
    /// means no limit.
    #[serde(default)]
    pub unhealthy_request_count: usize,
}

/// TLS certificate and key for incoming connections.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// Path to a PEM encoded certificate chain.
    pub cert: String,

    /// Path to a PEM encoded private key.
    pub key: String,
}

/// Per-pattern request customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestConfig {
    /// Header rules.
    #[serde(default)]
    pub headers: RequestHeaders,
}

/// Per-pattern response customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    /// Header rules.
    #[serde(default)]
    pub headers: ResponseHeaders,
}

/// Request header rules, applied before the request goes upstream.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// `Forwarded` header (RFC 7239) behavior.
    #[serde(default)]
    pub forwarded: ForwardedHeader,

    /// Headers to set, replacing existing values.
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Headers to remove.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Response header rules, applied before the response goes to the client.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// `Via` header behavior.
    #[serde(default)]
    pub via: ViaHeader,

    /// `Server` header behavior.
    #[serde(default)]
    pub server: ServerHeader,

    /// Headers to set, replacing existing values.
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Headers to remove.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// `Forwarded` header settings.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ForwardedHeader {
    /// Appends to client provided values instead of replacing them.
    #[serde(default = "default::true_value")]
    pub extend: bool,

    /// Value of the `by=` parameter. Defaults to the server name or address.
    pub by: Option<String>,
}

impl Default for ForwardedHeader {
    fn default() -> Self {
        Self {
            extend: true,
            by: None,
        }
    }
}

/// `Via` response header settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ViaHeader {
    /// Appends this proxy to the `Via` header of responses.
    #[serde(default)]
    pub extend: bool,
}

/// `Server` response header settings.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct ServerHeader {
    /// Replaces the upstream `Server` header with `rxh`.
    #[serde(rename = "override", default = "default::true_value")]
    pub should_override: bool,

    /// Includes the RXH version in the `Server` header.
    #[serde(default = "default::true_value")]
    pub version: bool,
}

impl Default for ServerHeader {
    fn default() -> Self {
        Self {
            should_override: true,
            version: true,
        }
    }
}

/// Response cache for `GET` requests.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Cache {
    /// Turns the cache on.
    #[serde(default)]
    pub enabled: bool,

    /// Lifetime of a cached response, in seconds.
    #[serde(default = "default::cache_ttl_seconds")]
    pub ttl_seconds: u64,

    /// Maximum number of cached responses.
    #[serde(default = "default::cache_max_entries")]
    pub max_entries: usize,

    /// Largest body stored, in bytes. Bigger responses stream uncached.
    #[serde(default = "default::cache_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: default::cache_ttl_seconds(),
            max_entries: default::cache_max_entries(),
            max_body_bytes: default::cache_max_body_bytes(),
        }
    }
}

/// Accepts `value` or `[value, ...]`.
fn one_or_many<'de, T, D>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<T> {
        One(T),
        Many(Vec<T>),
    }

    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(item) => vec![item],
        OneOrMany::Many(items) => items,
    })
}

mod default {
    //! Default values. They match Caddy's defaults where an equivalent exists.

    use std::net::SocketAddr;
    use std::time::Duration;

    pub fn uri() -> String {
        String::from("/")
    }

    pub fn admin_listen() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 2019))
    }

    pub fn max_connections() -> usize {
        1024
    }

    pub fn true_value() -> bool {
        true
    }

    pub fn one() -> u32 {
        1
    }

    pub fn weight() -> u32 {
        1
    }

    pub fn dynamic_refresh() -> Duration {
        Duration::from_secs(60)
    }

    pub fn choose() -> usize {
        2
    }

    pub fn try_interval() -> Duration {
        Duration::from_millis(250)
    }

    pub fn dial_timeout() -> Duration {
        Duration::from_secs(3)
    }

    pub fn keepalive() -> Duration {
        Duration::from_secs(120)
    }

    pub fn keepalive_idle_conns_per_host() -> usize {
        32
    }

    pub fn health_interval() -> Duration {
        Duration::from_secs(30)
    }

    pub fn health_timeout() -> Duration {
        Duration::from_secs(5)
    }

    pub fn health_method() -> String {
        String::from("GET")
    }

    pub fn fail_duration() -> Duration {
        Duration::from_secs(30)
    }

    pub fn udp_idle_timeout() -> Duration {
        Duration::from_secs(60)
    }

    pub fn cache_ttl_seconds() -> u64 {
        30
    }

    pub fn cache_max_entries() -> usize {
        1024
    }

    pub fn cache_max_body_bytes() -> usize {
        1024 * 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_ranges_keep_the_scheme() {
        assert_eq!(
            expand_port_range("https://h:1-2").unwrap(),
            ["https://h:1", "https://h:2"]
        );
        assert_eq!(expand_port_range("h:3").unwrap(), ["h:3"]);
        assert!(expand_port_range("h:2-1").is_err());
    }

    #[test]
    fn simple_forms() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            forward = "127.0.0.1:8080"

            [[server]]
            listen = ["127.0.0.1:9000", "127.0.0.1:9001"]
            serve = "/var/www"
            uri = "/static"
            "#,
        )
        .unwrap();

        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[1].listen.len(), 2);
        assert_eq!(config.servers[1].patterns[0].uri, "/static");
        assert!(config.admin.enabled);
    }

    #[test]
    fn named_groups_and_l4() {
        let config = Config::from_toml(
            r#"
            [upstream.db]
            policy = "least_conn"
            retries = 2
            try_duration = "5s"
            backends = ["10.0.0.1:5432", { address = "10.0.0.2:5432", weight = 3 }]

            [upstream.db.health]
            interval = "10s"

            [upstream.db.passive]
            fail_duration = "30s"
            unhealthy_status = ["5xx", 404]

            [[server]]
            listen = "0.0.0.0:5432"
            protocol = "tcp"
            proxy_protocol = "v2"
            forward = "@db"
            "#,
        )
        .unwrap();

        let group = &config.upstreams["db"];
        assert_eq!(group.policy, Policy::LeastConn);
        assert_eq!(group.try_duration, Some(Duration::from_secs(5)));
        assert_eq!(group.backends[1].weight, 3);
        assert_eq!(group.passive.as_ref().unwrap().unhealthy_status.len(), 2);
        assert!(
            matches!(&config.servers[0].patterns[0].action, Action::Forward(Forward::Group(name)) if name == "db")
        );
    }

    #[test]
    fn match_list_with_inline_policy() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8100"
            trusted_proxies = ["private_ranges"]
            match = [
                { uri = "/api", forward = { policy = "cookie", backends = ["127.0.0.1:8080"] } },
                { uri = "/", serve = "/srv" },
            ]
            "#,
        )
        .unwrap();

        assert_eq!(config.servers[0].patterns.len(), 2);
        assert!(!config.servers[0].trusted_proxies.is_empty());
    }

    #[test]
    fn legacy_algorithm_syntax() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8200"
            [server.forward]
            algorithm = "WRR"
            backends = [{ address = "127.0.0.1:8080", weight = 2 }]
            "#,
        )
        .unwrap();

        let Action::Forward(Forward::Inline(group)) = &config.servers[0].patterns[0].action else {
            panic!(
                "expected an inline group, got {:?}",
                config.servers[0].patterns[0].action
            );
        };
        assert_eq!(group.policy, Policy::WeightedRoundRobin);
    }

    #[test]
    fn rejects_bad_setups() {
        let cases = [
            // Unknown group.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"@nope\"",
            // HTTP only policy on TCP.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\nforward = { policy = \"uri_hash\", backends = [\"a:1\"] }",
            // Mixed actions.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"a:1\"\nserve = \"/\"",
            // Missing port.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"localhost\"",
            // Header policy without key.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = { policy = \"header\", backends = [\"a:1\"] }",
            // Typo in a field.
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforwrd = \"a:1\"",
        ];

        for case in cases {
            assert!(
                Config::from_toml(case).is_err(),
                "config should be rejected:\n{case}"
            );
        }
    }

    #[test]
    fn shared_listen_addresses() {
        let http = "[[server]]\nlisten = \"127.0.0.1:8000\"\nforward = \"a:1\"\n";
        let tls = "[[server]]\nlisten = \"127.0.0.1:8000\"\nforward = \"a:1\"\n\
                   tls = { cert = \"c.pem\", key = \"k.pem\" }\n";
        let tcp =
            "[[server]]\nlisten = \"127.0.0.1:8000\"\nprotocol = \"tcp\"\nforward = \"a:1\"\n";
        let udp =
            "[[server]]\nlisten = \"127.0.0.1:8000\"\nprotocol = \"udp\"\nforward = \"a:1\"\n";
        let error = |text: String| match Config::from_toml(&text) {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected a validation error, got {other:?}\n{text}"),
        };

        assert!(Config::from_toml(&format!("{http}{http}")).is_ok());
        assert!(Config::from_toml(&format!("{tls}{tls}")).is_ok());
        // TCP and UDP sockets never conflict.
        assert!(Config::from_toml(&format!("{tcp}{udp}")).is_ok());
        assert!(Config::from_toml(&format!("{http}{udp}")).is_ok());
        // Port 0 is a new free port every time.
        let zero = http.replace("8000", "0");
        let zero_tcp = tcp.replace("8000", "0");
        assert!(Config::from_toml(&format!("{zero}{zero_tcp}")).is_ok());

        assert!(error(format!("{http}{tcp}")).contains("only http servers can share"));
        assert!(error(format!("{tcp}{tcp}")).contains("only http servers can share"));
        assert!(error(format!("{udp}{udp}")).contains("only http servers can share"));
        assert!(error(format!("{http}{tls}")).contains("all or none"));
    }

    #[test]
    fn dynamic_upstreams_validation() {
        let parse = |dynamic: &str, extra: &str| {
            Config::from_toml(&format!(
                "[upstream.x{extra}]\nbackends = []\n[upstream.x.dynamic]\n{dynamic}\n"
            ))
        };
        let config = parse("a = \"app.local:80\"", "").unwrap();
        let dynamic = config.upstreams["x"].dynamic.as_ref().unwrap();
        assert_eq!(dynamic.refresh, Duration::from_secs(60));
        assert_eq!(dynamic.versions, None);
        assert!(
            parse(
                "srv = \"_http._tcp.app.local\"\nresolvers = [\"1.1.1.1:53\"]",
                ""
            )
            .is_ok()
        );

        for bad in [
            "",
            "a = \"x:80\"\nsrv = \"_a._b.c\"",
            "a = \"x\"",
            "a = \"https://x:80\"",
            "srv = \"x:80\"",
            "a = \"x:80\"\nrefresh = \"0s\"",
            "a = \"x:80\"\nversions = []",
            "srv = \"_a._b.c\"\nversions = [\"ipv4\"]",
        ] {
            assert!(parse(bad, "").is_err(), "{bad}");
        }

        let tls = "\n[upstream.x.tls]\ninsecure_skip_verify = true";
        let error = Config::from_toml(&format!(
            "[upstream.x]\nbackends = []\ndynamic = {{ a = \"x:80\" }}\n{}",
            tls
        ))
        .unwrap_err();
        assert!(error.to_string().contains("tls.server_name"), "{error}");
        assert!(
            Config::from_toml(&format!(
                "[upstream.x]\nbackends = []\ndynamic = {{ a = \"x:80\" }}\n{tls}\n\
                 server_name = \"x\""
            ))
            .is_ok()
        );
    }

    #[test]
    fn shipped_examples_are_valid() {
        let examples = [
            include_str!("../../../rxh.toml"),
            include_str!("../../../examples/full.toml"),
            include_str!("../../../examples/http-load-balancer.toml"),
            include_str!("../../../examples/tcp-udp.toml"),
        ];
        for example in examples {
            if let Err(err) = Config::from_toml(example) {
                panic!("example does not parse: {err}\n{example}");
            }
        }
    }

    #[test]
    fn port_ranges_expand() {
        let group = UpstreamGroup::with_backends(vec![Backend::new("localhost:8001-8003")]);
        let addresses: Vec<String> = group
            .expanded_backends()
            .unwrap()
            .into_iter()
            .map(|backend| backend.address)
            .collect();
        assert_eq!(
            addresses,
            ["localhost:8001", "localhost:8002", "localhost:8003"]
        );
    }

    #[test]
    fn policy_names_round_trip() {
        for policy in Policy::ALL {
            assert_eq!(policy.name().parse::<Policy>(), Ok(policy));
        }
        assert_eq!("WRR".parse::<Policy>(), Ok(Policy::WeightedRoundRobin));
    }
}
