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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde::de::IntoDeserializer;
use serde::{Deserialize, Serialize, Serializer};

use crate::address::{self, BackendAddress, Scheme};
use crate::duration;
use crate::matcher::{HostPattern, RequestMatcher};
use crate::net::IpNet;
use crate::size;
use crate::status::StatusMatcher;
use crate::template::Template;

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

    /// Access log settings.
    #[serde(default)]
    pub log: Log,

    /// Certificate authority settings for `tls = "auto"` servers.
    #[serde(default)]
    pub acme: Acme,

    /// Routes to Docker containers, taken from their `rxh.*` labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docker: Option<Docker>,

    /// Routes to the healthy instances of the Consul catalog services tagged
    /// `rxh.enable=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consul: Option<Catalog>,

    /// Routes to the service instances written under a prefix of etcd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etcd: Option<Catalog>,

    /// Routes to the Nomad services tagged `rxh.enable=true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nomad: Option<Catalog>,

    /// Routes the hosts of the Kubernetes Ingresses and Gateway API
    /// HTTPRoutes to the ready endpoints of their Services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<Catalog>,

    /// Maximum time to wait for open connections during shutdown or reload.
    /// `None` waits forever, like Caddy's default.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub grace_period: Option<Duration>,

    /// How the listeners are spread over the CPUs.
    #[serde(default, skip_serializing_if = "Runtime::is_shared")]
    pub runtime: Runtime,

    /// Reload when the configuration file or one of its included files
    /// changes, as `rxh reload` would.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub watch: bool,

    /// Files merged into this one, relative to the directory of the main
    /// file. `*` and `?` work in the last path component. Followed by the
    /// loader of the daemon, which empties this list after merging.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<String>,

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

/// Content of an included file: only pools and servers.
///
/// ```toml
/// [upstream.api]
/// backends = ["127.0.0.1:8080"]
///
/// [[server]]
/// listen = "0.0.0.0:80"
/// forward = "@api"
/// ```
#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct IncludedConfig {
    /// Pools, merged into [`Config::upstreams`].
    #[serde(default, rename = "upstream")]
    pub upstreams: BTreeMap<String, UpstreamGroup>,

    /// Servers, appended to [`Config::servers`].
    #[serde(default, rename = "server")]
    pub servers: Vec<Server>,
}

impl IncludedConfig {
    /// Parses an included file. Any table other than `upstream` and `server`
    /// is an error.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }
}

impl Config {
    /// Parses and validates a TOML document. `include` is not followed, use
    /// [`Config::parse`] and [`Config::merge`] for that.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config = Self::parse(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Parses a TOML document without validating it, so included files can
    /// be merged first.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(text)?)
    }

    /// Adds the pools and servers of an included file, named `origin` in
    /// error messages. A pool name defined twice is an error.
    pub fn merge(&mut self, included: IncludedConfig, origin: &str) -> Result<(), ConfigError> {
        for (name, group) in included.upstreams {
            if self.upstreams.contains_key(&name) {
                return Err(ConfigError::Invalid(format!(
                    "{origin}: pool {name} is already defined in another file"
                )));
            }
            self.upstreams.insert(name, group);
        }
        self.servers.extend(included.servers);
        Ok(())
    }

    /// Checks references and protocol constraints that serde cannot express.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: String| Err(ConfigError::Invalid(message));

        if self.log.output.trim().is_empty() {
            return invalid(String::from("log.output must not be empty"));
        }
        self.admin
            .validate()
            .map_err(|err| ConfigError::Invalid(format!("admin: {err}")))?;
        self.acme
            .validate()
            .map_err(|err| ConfigError::Invalid(format!("acme: {err}")))?;
        if let Some(docker) = &self.docker {
            docker
                .validate()
                .map_err(|err| ConfigError::Invalid(format!("docker: {err}")))?;
        }
        for kind in CatalogKind::ALL {
            if let Some(catalog) = self.catalog(kind) {
                catalog
                    .validate(kind)
                    .map_err(|err| ConfigError::Invalid(format!("{}: {err}", kind.section())))?;
            }
        }

        for (name, group) in &self.upstreams {
            group
                .validate()
                .map_err(|err| ConfigError::Invalid(format!("upstream.{name}: {err}")))?;
        }

        // Servers sharing a listen address, keyed by address and "is UDP".
        let mut listeners: HashMap<(SocketAddr, bool), (String, &Server)> = HashMap::new();
        // Server names: inline pools, metrics, logs and `rxh service` use them as keys.
        let mut names = HashMap::new();

        for (index, server) in self.servers.iter().enumerate() {
            let label = server.label(index);

            if let Some(name) = &server.name
                && let Some(first) = names.insert(name, index)
            {
                return invalid(format!(
                    "{label}: the name {name:?} is already used by server[{first}]"
                ));
            }

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
                // One handshake configuration per listener: a policy picked by
                // SNI could be dodged with a `Host` header naming another server.
                if server.client_auth != first.client_auth {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         the servers on it must have the same client_auth"
                    ));
                }
                if server.tls_options != first.tls_options {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         the servers on it must have the same tls_options"
                    ));
                }
                if server.proxy_protocol_in != first.proxy_protocol_in {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         either all or none of the servers on it must set proxy_protocol_in"
                    ));
                }
                if server.max_connections_per_ip != first.max_connections_per_ip {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         the servers on it must have the same max_connections_per_ip"
                    ));
                }
                // The certificate of a host-less server is the listener's default
                // only when it is set first; later ones would never be served.
                if server.tls.is_some() && server.host.is_empty() {
                    return invalid(format!(
                        "{label}: listen address {address} is shared with {first_label}, \
                         only the first TLS server on it may omit host"
                    ));
                }
            }

            for (name, value) in [
                ("read_header_timeout", server.read_header_timeout),
                ("idle_timeout", server.idle_timeout),
                ("stream_timeout", server.stream_timeout),
                ("match_timeout", Some(server.match_timeout)),
            ] {
                if value.is_some_and(|value| value.is_zero()) {
                    return invalid(format!("{label}: {name} must be greater than zero"));
                }
            }

            if server.protocol != Protocol::Http && server.stream_close_delay.is_some() {
                return invalid(format!(
                    "{label}: stream_close_delay only applies to http servers"
                ));
            }

            if server.protocol != Protocol::Http && server.rate_limit.is_some() {
                return invalid(format!("{label}: rate_limit only applies to http servers"));
            }
            if let Some(rate_limit) = &server.rate_limit {
                rate_limit
                    .validate()
                    .map_err(|err| ConfigError::Invalid(format!("{label}: rate_limit: {err}")))?;
            }
            validate_auth(
                server.protocol,
                server.basic_auth.as_ref(),
                server.forward_auth.as_ref(),
            )
            .map_err(|err| ConfigError::Invalid(format!("{label}: {err}")))?;
            validate_response_policy(
                server.protocol,
                server.security_headers,
                server.cors.as_ref(),
            )
            .map_err(|err| ConfigError::Invalid(format!("{label}: {err}")))?;
            match server.max_connections_per_ip {
                Some(0) => {
                    return invalid(format!(
                        "{label}: max_connections_per_ip must be greater than zero"
                    ));
                }
                Some(_) if server.protocol == Protocol::Udp => {
                    return invalid(format!(
                        "{label}: max_connections_per_ip does not apply to udp servers"
                    ));
                }
                _ => {}
            }

            if server.protocol == Protocol::Udp && !server.host.is_empty() {
                return invalid(format!("{label}: host does not apply to udp servers"));
            }

            for (pattern_index, pattern) in server.patterns.iter().enumerate() {
                if server.protocol != Protocol::Http && pattern.has_http_options() {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: rewrite, request, response, \
                         handle_response, mirror, encode and cache only apply to http servers"
                    ));
                }
                if server.protocol != Protocol::Http
                    && (!pattern.ip_allow.is_empty() || !pattern.ip_deny.is_empty())
                {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: ip_allow and ip_deny inside match \
                         entries only apply to http servers, set them on the server"
                    ));
                }
                if let Some(rate_limit) = &pattern.rate_limit {
                    if server.protocol != Protocol::Http {
                        return invalid(format!(
                            "{label}: match[{pattern_index}]: rate_limit only applies to http \
                             servers"
                        ));
                    }
                    rate_limit.validate().map_err(|err| {
                        ConfigError::Invalid(format!(
                            "{label}: match[{pattern_index}]: rate_limit: {err}"
                        ))
                    })?;
                }
                validate_auth(
                    server.protocol,
                    pattern.basic_auth.as_ref(),
                    pattern.forward_auth.as_ref(),
                )
                .map_err(|err| {
                    ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                })?;
                validate_response_policy(
                    server.protocol,
                    pattern.security_headers,
                    pattern.cors.as_ref(),
                )
                .map_err(|err| {
                    ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                })?;
                pattern.validate_rewrite().map_err(|err| {
                    ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                })?;
                if server.protocol == Protocol::Udp && pattern.has_http_matchers() {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: udp servers take no request matchers"
                    ));
                }
                if server.protocol == Protocol::Tcp {
                    if pattern.has_http_matchers() {
                        return invalid(format!(
                            "{label}: match[{pattern_index}]: tcp servers match with sni, \
                             alpn, protocol and http_host, not with HTTP request matchers"
                        ));
                    }
                    for host in pattern.sni.iter().chain(&pattern.http_host) {
                        HostPattern::new(host).map_err(|err| {
                            ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                        })?;
                    }
                } else if pattern.has_l4_matchers() {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: sni, alpn, protocol and http_host \
                         only apply to tcp servers"
                    ));
                }
                RequestMatcher::new(pattern, &server.host).map_err(|err| {
                    ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                })?;
                pattern.request.headers.validate().map_err(|err| {
                    ConfigError::Invalid(format!(
                        "{label}: match[{pattern_index}]: request.headers: {err}"
                    ))
                })?;
                pattern.response.headers.validate().map_err(|err| {
                    ConfigError::Invalid(format!(
                        "{label}: match[{pattern_index}]: response.headers: {err}"
                    ))
                })?;
                if !pattern.handle_response.is_empty()
                    && !matches!(pattern.action, Action::Forward(_))
                {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: handle_response only applies to forward"
                    ));
                }
                if !pattern.mirror.is_empty() && !matches!(pattern.action, Action::Forward(_)) {
                    return invalid(format!(
                        "{label}: match[{pattern_index}]: mirror only applies to forward"
                    ));
                }
                for (mirror_index, mirror) in pattern.mirror.iter().enumerate() {
                    let at = format!("{label}: match[{pattern_index}]: mirror[{mirror_index}]");
                    if mirror.percent > 100 {
                        return invalid(format!("{at}: percent must be between 0 and 100"));
                    }
                    let Some(group) = self.upstreams.get(&mirror.pool) else {
                        return invalid(format!("{at}: unknown upstream group @{}", mirror.pool));
                    };
                    // Mirrors get the same PROXY header as the forward.
                    if server.proxy_protocol.is_some() && group.h2c {
                        return invalid(format!(
                            "{at}: proxy_protocol cannot be used with h2c backends"
                        ));
                    }
                }
                for (handler_index, handler) in pattern.handle_response.iter().enumerate() {
                    handler.validate().map_err(|err| {
                        ConfigError::Invalid(format!(
                            "{label}: match[{pattern_index}]: handle_response[{handler_index}]: \
                             {err}"
                        ))
                    })?;
                }
                if let Action::Serve(files) = &pattern.action {
                    files.validate().map_err(|err| {
                        ConfigError::Invalid(format!("{label}: match[{pattern_index}]: {err}"))
                    })?;
                    continue;
                }
                if let Action::Redirect(redirect) = &pattern.action {
                    redirect.validate().map_err(|err| {
                        ConfigError::Invalid(format!(
                            "{label}: match[{pattern_index}]: redirect: {err}"
                        ))
                    })?;
                    continue;
                }
                let Action::Forward(forward) = &pattern.action else {
                    continue;
                };

                let groups: Vec<&UpstreamGroup> = match forward {
                    Forward::Group(name) => match self.upstreams.get(name) {
                        Some(group) => vec![group],
                        None => return invalid(format!("{label}: unknown upstream group @{name}")),
                    },
                    Forward::Inline(group) => {
                        group.validate().map_err(|err| {
                            ConfigError::Invalid(format!("{label}: forward: {err}"))
                        })?;
                        if group.backends.is_empty() && group.dynamic.is_none() {
                            return invalid(format!("{label}: forward needs at least one backend"));
                        }
                        vec![&**group]
                    }
                    Forward::Split(split) => {
                        if server.protocol == Protocol::Udp {
                            return invalid(format!(
                                "{label}: udp servers cannot split traffic between pools"
                            ));
                        }
                        if split.iter().all(|target| target.weight == 0) {
                            return invalid(format!(
                                "{label}: forward: at least one pool needs a weight above 0"
                            ));
                        }
                        let mut groups = Vec::with_capacity(split.len());
                        for (index, target) in split.iter().enumerate() {
                            if split[..index].iter().any(|other| other.pool == target.pool) {
                                return invalid(format!(
                                    "{label}: forward: pool {} is listed twice",
                                    target.pool
                                ));
                            }
                            match self.upstreams.get(&target.pool) {
                                Some(group) => groups.push(group),
                                None => {
                                    return invalid(format!(
                                        "{label}: unknown upstream group @{}",
                                        target.pool
                                    ));
                                }
                            }
                        }
                        groups
                    }
                };

                for group in groups {
                    // An h2c backend shares one connection between all clients, and a
                    // PROXY header describes a single one.
                    if server.protocol == Protocol::Http
                        && server.proxy_protocol.is_some()
                        && group.h2c
                    {
                        return invalid(format!(
                            "{label}: proxy_protocol cannot be used with h2c backends"
                        ));
                    }

                    if server.protocol != Protocol::Http && group.policy.requires_http() {
                        return invalid(format!(
                            "{label}: policy {} needs HTTP requests, it cannot be used with {}",
                            group.policy, server.protocol
                        ));
                    }
                }
            }

            if server.protocol == Protocol::Udp && server.patterns.len() != 1 {
                return invalid(format!("{label}: udp servers take a single forward"));
            }
            if server.protocol != Protocol::Http
                && server
                    .patterns
                    .iter()
                    .any(|pattern| !matches!(pattern.action, Action::Forward(_)))
            {
                return invalid(format!(
                    "{label}: {} servers can only forward",
                    server.protocol
                ));
            }

            if server.protocol == Protocol::Udp
                && let Some(Action::Forward(forward)) = server.patterns.first().map(|p| &p.action)
                && let Some(group) = match forward {
                    Forward::Group(name) => self.upstreams.get(name),
                    Forward::Inline(group) => Some(&**group),
                    Forward::Split(_) => None,
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

            if let Some(client_auth) = &server.client_auth {
                if server.tls.is_none() {
                    return invalid(format!("{label}: client_auth needs tls"));
                }
                if client_auth.header.is_some() && server.protocol != Protocol::Http {
                    return invalid(format!(
                        "{label}: client_auth.header only applies to http servers"
                    ));
                }
                client_auth
                    .validate()
                    .map_err(|err| ConfigError::Invalid(format!("{label}: client_auth: {err}")))?;
            }

            if let Some(tls_options) = &server.tls_options {
                if server.tls.is_none() {
                    return invalid(format!("{label}: tls_options needs tls"));
                }
                tls_options
                    .validate()
                    .map_err(|err| ConfigError::Invalid(format!("{label}: tls_options: {err}")))?;
            }

            if server.tls == Some(Tls::Auto) {
                if server.protocol != Protocol::Http {
                    return invalid(format!(
                        "{label}: tls = \"auto\" only applies to http servers"
                    ));
                }
                if server.host.is_empty() {
                    return invalid(format!(
                        "{label}: tls = \"auto\" needs the host names to certify in host"
                    ));
                }
                for host in &server.host {
                    // Only DNS-01 can validate a wildcard.
                    let name = match host.strip_prefix("*.") {
                        Some(base) if self.acme.challenge == AcmeChallenge::Dns01 => base,
                        _ => host,
                    };
                    validate_domain(name).map_err(|err| {
                        ConfigError::Invalid(format!("{label}: tls = \"auto\": {err}"))
                    })?;
                }
            }

            if server.https_redirect == Some(true) {
                if server.protocol != Protocol::Http || server.tls.is_none() {
                    return invalid(format!(
                        "{label}: https_redirect only applies to http servers with tls"
                    ));
                }
                if server.listen.iter().all(|address| address.port() == 0) {
                    return invalid(format!(
                        "{label}: https_redirect needs a listen address with a fixed port"
                    ));
                }
                if let Some(blocker) = self.http_port_blocker() {
                    return invalid(format!(
                        "{label}: https_redirect needs port {} ([acme] http_port), which is \
                         used by {blocker}; only plain http servers can share it",
                        self.acme.http_port
                    ));
                }
            }

            if server.proxy_protocol.is_some() && server.protocol == Protocol::Udp {
                return invalid(format!(
                    "{label}: proxy_protocol only applies to http and tcp servers"
                ));
            }

            if server.proxy_protocol_in && server.protocol == Protocol::Udp {
                return invalid(format!(
                    "{label}: proxy_protocol_in only applies to http and tcp servers"
                ));
            }
        }

        Ok(())
    }

    /// Label of the first server on `[acme] http_port` that is not a plain
    /// `http` server, which keeps HTTP to HTTPS redirects off that port.
    fn http_port_blocker(&self) -> Option<String> {
        let port = self.acme.http_port;
        self.servers
            .iter()
            .enumerate()
            .find(|(_, server)| {
                server.protocol != Protocol::Udp
                    && (server.protocol != Protocol::Http || server.tls.is_some())
                    && server.listen.iter().any(|address| address.port() == port)
            })
            .map(|(index, server)| server.label(index))
    }

    /// The `[consul]`, `[etcd]`, `[nomad]` or `[kubernetes]` section, if
    /// present.
    #[must_use]
    pub fn catalog(&self, kind: CatalogKind) -> Option<&Catalog> {
        match kind {
            CatalogKind::Consul => self.consul.as_ref(),
            CatalogKind::Etcd => self.etcd.as_ref(),
            CatalogKind::Nomad => self.nomad.as_ref(),
            CatalogKind::Kubernetes => self.kubernetes.as_ref(),
        }
    }

    /// Returns this configuration plus one plain `http` server per TLS
    /// server that redirects HTTP to HTTPS (`https_redirect`, on by default
    /// with `tls = "auto"`). It listens on the addresses of the TLS server
    /// with the port replaced by `[acme] http_port`, shares them with the
    /// plain `http` servers already there (whose routes go first) and
    /// answers `308` to `https://{host}{uri}`.
    ///
    /// Nothing is added while a TLS or `tcp` server uses `http_port`, nor
    /// for listen addresses with port 0.
    #[must_use]
    pub fn with_https_redirects(&self) -> Self {
        let mut config = self.clone();
        if self.http_port_blocker().is_some() {
            return config;
        }
        for server in &self.servers {
            let enabled = server
                .https_redirect
                .unwrap_or(server.tls == Some(Tls::Auto));
            if !enabled || server.tls.is_none() || server.protocol != Protocol::Http {
                continue;
            }
            let mut listen: Vec<SocketAddr> = Vec::new();
            let mut https_port = None;
            for address in server.listen.iter().filter(|address| address.port() != 0) {
                https_port.get_or_insert(address.port());
                let http = SocketAddr::new(address.ip(), self.acme.http_port);
                if !listen.contains(&http) {
                    listen.push(http);
                }
            }
            let Some(https_port) = https_port else {
                continue;
            };
            let to = if https_port == 443 {
                String::from("https://{host}{uri}")
            } else {
                format!("https://{{host}}:{https_port}{{uri}}")
            };
            config.servers.push(Server {
                listen,
                patterns: vec![Pattern::new(Action::Redirect(Redirect { to, status: 308 }))],
                name: None,
                tls: None,
                client_auth: None,
                tls_options: None,
                https_redirect: None,
                ..server.clone()
            });
        }
        config
    }
}

/// Where the admin API listens: `ip:port` or `unix//absolute/path.sock`
/// (Caddy syntax, like backend addresses).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub enum AdminListen {
    /// TCP address.
    Tcp(SocketAddr),

    /// Unix domain socket at this absolute path.
    Unix(PathBuf),
}

impl FromStr for AdminListen {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if let Some(path) = text.strip_prefix("unix/") {
            return if path.len() > 1 && path.starts_with('/') {
                Ok(Self::Unix(PathBuf::from(path)))
            } else {
                Err(format!(
                    "invalid admin address {text:?}, the socket path must be absolute: \
                     unix//run/rxh/admin.sock"
                ))
            };
        }
        text.parse().map(Self::Tcp).map_err(|_| {
            format!("invalid admin address {text:?}, expected ip:port or unix//absolute/path")
        })
    }
}

impl TryFrom<String> for AdminListen {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl fmt::Display for AdminListen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tcp(address) => write!(f, "{address}"),
            Self::Unix(path) => write!(f, "unix/{}", path.display()),
        }
    }
}

impl From<AdminListen> for String {
    fn from(listen: AdminListen) -> Self {
        listen.to_string()
    }
}

/// Shortest accepted `[admin] token`.
const MIN_ADMIN_TOKEN_LENGTH: usize = 16;

/// Admin API settings. The admin API is how the CLI talks to a running daemon.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    /// Disables the admin API when false.
    #[serde(default = "default::true_value")]
    pub enabled: bool,

    /// Address of the admin API: a TCP address, on loopback unless `token`
    /// is set, or a unix socket only its owner can open.
    #[serde(default = "default::admin_listen")]
    pub listen: AdminListen,

    /// Secret every request must carry as `Authorization: Bearer <token>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl Default for Admin {
    fn default() -> Self {
        Self {
            enabled: true,
            listen: default::admin_listen(),
            token: None,
        }
    }
}

impl Admin {
    /// Replaces the token with `***`, for output that leaves the process.
    pub fn redact(&mut self) {
        if let Some(token) = &mut self.token {
            *token = String::from("***");
        }
    }

    fn validate(&self) -> Result<(), String> {
        if let Some(token) = &self.token {
            if token.len() < MIN_ADMIN_TOKEN_LENGTH {
                return Err(format!(
                    "token must have at least {MIN_ADMIN_TOKEN_LENGTH} characters"
                ));
            }
            // It travels in an `Authorization` header.
            if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(String::from("token must be printable ASCII without spaces"));
            }
        }
        if let AdminListen::Tcp(address) = &self.listen
            && self.enabled
            && self.token.is_none()
            && !address.ip().is_loopback()
        {
            return Err(format!(
                "listen = \"{address}\" is reachable from other machines, set token or use a \
                 loopback address or a unix socket"
            ));
        }
        Ok(())
    }
}

/// Line format of access logs.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human readable one line per request.
    #[default]
    Text,

    /// One JSON object per line.
    Json,
}

/// Access log settings, the `[log]` table.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Log {
    /// Format of access lines.
    #[serde(default)]
    pub format: LogFormat,

    /// `"stdout"`, `"stderr"` or the path of a file that is appended to.
    #[serde(default = "default::log_output")]
    pub output: String,

    /// Writes one line per request or connection when true.
    #[serde(default = "default::true_value")]
    pub access: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            format: LogFormat::Text,
            output: default::log_output(),
            access: true,
        }
    }
}

/// Docker provider (`[docker]`): running containers labelled `rxh.enable=true`
/// become pools and servers, followed live through the Docker events, or the
/// tasks of the Swarm services labelled `rxh.enable=true`, polled.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Docker {
    /// Engine that runs the containers.
    #[serde(default)]
    pub provider: ContainerProvider,

    /// Engine API: `unix:///path/to/docker.sock` or `tcp://host:port`.
    /// Defaults to the system socket of the provider, see [`Docker::endpoint`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Address of the HTTP servers made from `rxh.http.host` labels.
    #[serde(default = "default::docker_listen")]
    pub listen: SocketAddr,

    /// How often the Swarm services and tasks are listed. Docker and Podman
    /// containers are followed through events instead.
    #[serde(default = "default::docker_refresh", with = "duration")]
    pub refresh: Duration,
}

/// Engine behind `[docker]`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ContainerProvider {
    /// Docker Engine: containers reached at their address on their network.
    #[default]
    Docker,
    /// Podman, through its Docker compatible API: containers reached at the
    /// host port they publish, which rootless containers need, else at
    /// their address on their network.
    Podman,
    /// Docker Swarm mode: the running tasks of the services, reached at
    /// their address on their first network other than `ingress`.
    Swarm,
}

impl Default for Docker {
    fn default() -> Self {
        Self {
            provider: ContainerProvider::default(),
            endpoint: None,
            listen: default::docker_listen(),
            refresh: default::docker_refresh(),
        }
    }
}

impl Docker {
    /// The `endpoint` set, or the system socket of the provider.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        match (&self.endpoint, self.provider) {
            (Some(endpoint), _) => endpoint,
            (None, ContainerProvider::Docker | ContainerProvider::Swarm) => {
                "unix:///var/run/docker.sock"
            }
            (None, ContainerProvider::Podman) => "unix:///run/podman/podman.sock",
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.refresh.is_zero() {
            return Err(String::from("refresh must be longer than 0s"));
        }
        let endpoint = self.endpoint();
        let valid = match endpoint.split_once("://") {
            Some(("unix", path)) => path.starts_with('/'),
            Some(("tcp", address)) => matches!(
                BackendAddress::parse(address),
                Ok(BackendAddress::Tcp {
                    scheme: Scheme::Bare,
                    ..
                })
            ),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(format!(
                "endpoint must be unix:///path or tcp://host:port, got {endpoint:?}"
            ))
        }
    }
}

/// Service catalog provider: `[consul]`, `[etcd]`, `[nomad]` or
/// `[kubernetes]`. Each service instance becomes a member of a pool, with
/// the `rxh.*` options of its tags (Consul, Nomad) or keys (etcd), like the
/// labels of `[docker]`; in Kubernetes the hosts come from the Ingresses and
/// HTTPRoutes and the instances are the ready endpoints of their Services.
/// The catalog is listed every `refresh`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// HTTP API: `http://host:port`. Defaults to the local agent, see
    /// [`Catalog::endpoint`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// ACL token sent to Consul (`X-Consul-Token`) or Nomad
    /// (`X-Nomad-Token`), or bearer token sent to the Kubernetes API. Not
    /// accepted by `[etcd]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Keys of the service instances in etcd. Only accepted by `[etcd]`,
    /// where it defaults to `rxh/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,

    /// Only the Ingresses of this class (`spec.ingressClassName` or the
    /// `kubernetes.io/ingress.class` annotation) are routed. Only accepted
    /// by `[kubernetes]`, where every Ingress is routed without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_class: Option<String>,

    /// Address of the HTTP servers made from `rxh.http.host` options.
    #[serde(default = "default::docker_listen")]
    pub listen: SocketAddr,

    /// How often the catalog is listed.
    #[serde(default = "default::docker_refresh", with = "duration")]
    pub refresh: Duration,
}

/// Which service catalog a [`Catalog`] is the section of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogKind {
    /// `[consul]`.
    Consul,
    /// `[etcd]`.
    Etcd,
    /// `[nomad]`.
    Nomad,
    /// `[kubernetes]`.
    Kubernetes,
}

impl CatalogKind {
    /// Every kind.
    pub const ALL: [Self; 4] = [Self::Consul, Self::Etcd, Self::Nomad, Self::Kubernetes];

    /// Name of the section in the configuration file.
    #[must_use]
    pub fn section(self) -> &'static str {
        match self {
            Self::Consul => "consul",
            Self::Etcd => "etcd",
            Self::Nomad => "nomad",
            Self::Kubernetes => "kubernetes",
        }
    }

    /// Port of the HTTP API of a local agent, or of `kubectl proxy`.
    #[must_use]
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Self::Consul => "http://127.0.0.1:8500",
            Self::Etcd => "http://127.0.0.1:2379",
            Self::Nomad => "http://127.0.0.1:4646",
            Self::Kubernetes => "http://127.0.0.1:8001",
        }
    }
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            endpoint: None,
            token: None,
            prefix: None,
            ingress_class: None,
            listen: default::docker_listen(),
            refresh: default::docker_refresh(),
        }
    }
}

impl Catalog {
    /// The `endpoint` set, or the local agent of `kind`.
    #[must_use]
    pub fn endpoint(&self, kind: CatalogKind) -> &str {
        self.endpoint
            .as_deref()
            .unwrap_or_else(|| kind.default_endpoint())
    }

    /// The `prefix` set, or `rxh/`.
    #[must_use]
    pub fn prefix(&self) -> &str {
        self.prefix.as_deref().unwrap_or("rxh/")
    }

    fn validate(&self, kind: CatalogKind) -> Result<(), String> {
        if self.refresh.is_zero() {
            return Err(String::from("refresh must be longer than 0s"));
        }
        let endpoint = self.endpoint(kind);
        let valid = endpoint.starts_with("http://")
            && matches!(
                BackendAddress::parse(endpoint),
                Ok(BackendAddress::Tcp { host, .. }) if !host.contains('/')
            );
        if !valid {
            return Err(format!(
                "endpoint must be http://host:port, got {endpoint:?}"
            ));
        }
        match (kind, self.ingress_class.as_deref()) {
            (CatalogKind::Kubernetes, Some("")) => {
                return Err(String::from("ingress_class cannot be empty"));
            }
            (CatalogKind::Kubernetes, _) | (_, None) => {}
            (_, Some(_)) => {
                return Err(String::from("ingress_class is only used by [kubernetes]"));
            }
        }
        match (kind, &self.token, &self.prefix) {
            (CatalogKind::Etcd, Some(_), _) => Err(String::from("token is not supported")),
            (CatalogKind::Etcd, _, Some(prefix)) if prefix.is_empty() => {
                Err(String::from("prefix cannot be empty"))
            }
            (CatalogKind::Etcd, ..) | (_, _, None) => Ok(()),
            (_, _, Some(_)) => Err(String::from("prefix is only used by [etcd]")),
        }
    }
}

/// Threading model of the listeners.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Runtime {
    /// One work-stealing runtime: a connection may move between threads.
    #[default]
    Shared,

    /// One single-threaded runtime per CPU, each with its own `SO_REUSEPORT`
    /// listeners and upstream connections: a connection stays on the thread
    /// that accepted it.
    PerCore,
}

impl Runtime {
    fn is_shared(&self) -> bool {
        *self == Runtime::Shared
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

/// PROXY protocol header sent to upstreams on new connections of `http` and
/// `tcp` servers.
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

    /// Client certificates asked for in the TLS handshake (mutual TLS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_auth: Option<ClientAuth>,

    /// Protocol versions, cipher suites and curves of the TLS handshake, and
    /// OCSP stapling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_options: Option<TlsOptions>,

    /// Redirects plain HTTP requests for this TLS server to HTTPS, see
    /// [`Config::with_https_redirects`]. Unset means on with `tls = "auto"`
    /// and off otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub https_redirect: Option<bool>,

    /// Accepts HTTP/2 (h2 over TLS and h2c) on this listener.
    pub http2: bool,

    /// Peers allowed to set `X-Forwarded-*` headers and the real client IP.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trusted_proxies: Vec<IpNet>,

    /// Client networks accepted by every route of the server, see
    /// [`crate::net::permitted`]. Empty accepts any client not in `ip_deny`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ip_allow: Vec<IpNet>,

    /// Client networks rejected by every route of the server.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ip_deny: Vec<IpNet>,

    /// Request rate limit shared by every route of the server (http only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// HTTP Basic authentication of every route that does not set its own
    /// (http only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuth>,

    /// Authentication delegated to an external service, for every route that
    /// does not set its own (http only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_auth: Option<ForwardAuth>,

    /// Preset of security response headers of every route that does not set
    /// its own (http only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_headers: Option<SecurityHeaders>,

    /// Cross-origin resource sharing of every route that does not set its
    /// own (http only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,

    /// Maximum number of concurrent connections of one client IP (http and
    /// tcp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connections_per_ip: Option<usize>,

    /// PROXY protocol header to send upstream (tcp only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_protocol: Option<ProxyProtocol>,

    /// Requires a PROXY protocol header (v1 or v2) at the start of every
    /// connection and uses its source address as the client address.
    pub proxy_protocol_in: bool,

    /// Time a client has to send the incoming PROXY header and, on `tcp`
    /// servers with layer 4 matchers, the bytes the matchers look at.
    #[serde(with = "duration")]
    pub match_timeout: Duration,

    /// Idle time after which a UDP session is dropped.
    #[serde(with = "duration")]
    pub udp_idle_timeout: Duration,

    /// Time a client has to send the request headers (http only).
    #[serde(skip_serializing_if = "Option::is_none", with = "duration::option")]
    pub read_header_timeout: Option<Duration>,

    /// Idle time after which a keep-alive HTTP connection or a `tcp`
    /// connection is closed.
    #[serde(skip_serializing_if = "Option::is_none", with = "duration::option")]
    pub idle_timeout: Option<Duration>,

    /// Maximum lifetime of upgraded HTTP tunnels and `tcp` connections.
    #[serde(skip_serializing_if = "Option::is_none", with = "duration::option")]
    pub stream_timeout: Option<Duration>,

    /// Time upgraded HTTP tunnels stay open once the listener shuts down or
    /// is replaced by a reload. Unset keeps them open until they end.
    #[serde(skip_serializing_if = "Option::is_none", with = "duration::option")]
    pub stream_close_delay: Option<Duration>,
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
    serve: Option<FileServer>,
    redirect: Option<Redirect>,
    rewrite: Option<String>,
    #[serde(default)]
    encode: Vec<Encoding>,
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
    #[serde(default)]
    handle_response: Vec<HandleResponse>,
    #[serde(default, deserialize_with = "one_or_many")]
    mirror: Vec<Mirror>,
    cache: Option<Cache>,
    #[serde(default = "default::max_connections", alias = "connections")]
    max_connections: usize,
    name: Option<String>,
    tls: Option<Tls>,
    client_auth: Option<ClientAuth>,
    tls_options: Option<TlsOptions>,
    https_redirect: Option<bool>,
    #[serde(default = "default::true_value")]
    http2: bool,
    #[serde(default)]
    trusted_proxies: Vec<String>,
    #[serde(default)]
    ip_allow: Vec<IpNet>,
    #[serde(default)]
    ip_deny: Vec<IpNet>,
    rate_limit: Option<RateLimit>,
    basic_auth: Option<BasicAuth>,
    forward_auth: Option<ForwardAuth>,
    security_headers: Option<SecurityHeaders>,
    cors: Option<Cors>,
    max_connections_per_ip: Option<usize>,
    proxy_protocol: Option<ProxyProtocol>,
    #[serde(default)]
    proxy_protocol_in: bool,
    #[serde(default = "default::match_timeout", with = "duration")]
    match_timeout: Duration,
    #[serde(default = "default::udp_idle_timeout", with = "duration")]
    udp_idle_timeout: Duration,
    #[serde(default, with = "duration::option")]
    read_header_timeout: Option<Duration>,
    #[serde(default, with = "duration::option")]
    idle_timeout: Option<Duration>,
    #[serde(default, with = "duration::option")]
    stream_timeout: Option<Duration>,
    #[serde(default, with = "duration::option")]
    stream_close_delay: Option<Duration>,
}

impl TryFrom<RawServer> for Server {
    type Error = String;

    fn try_from(raw: RawServer) -> Result<Self, Self::Error> {
        if raw.listen.is_empty() {
            return Err(String::from("missing field `listen`"));
        }

        let inline_action = match (raw.forward, raw.serve, raw.redirect) {
            (Some(forward), None, None) => Some(Action::Forward(forward)),
            (None, Some(root), None) => Some(Action::Serve(root)),
            (None, None, Some(redirect)) => Some(Action::Redirect(redirect)),
            (None, None, None) => None,
            _ => {
                return Err(String::from(
                    "use either 'forward', 'serve' or 'redirect', if you need multiple patterns \
                     use 'match'",
                ));
            }
        };

        let has_inline_options = raw.uri.is_some()
            || !raw.path.is_empty()
            || !raw.method.is_empty()
            || !raw.header.is_empty()
            || !raw.query.is_empty()
            || !raw.remote_ip.is_empty()
            || raw.rewrite.is_some()
            || raw.request.is_some()
            || raw.response.is_some()
            || !raw.handle_response.is_empty()
            || !raw.mirror.is_empty()
            || !raw.encode.is_empty()
            || raw.cache.is_some();

        let patterns = match (raw.patterns, inline_action) {
            (Some(_), Some(_)) => {
                return Err(String::from(
                    "either use 'match' for multiple patterns or describe a single pattern",
                ));
            }
            (Some(_), None) if has_inline_options => {
                return Err(String::from(
                    "'uri', 'path', 'method', 'header', 'query', 'remote_ip', 'rewrite', \
                     'request', 'response', 'handle_response', 'mirror', 'encode' and 'cache' \
                     go inside each 'match' entry",
                ));
            }
            (Some(patterns), None) if !patterns.is_empty() => patterns,
            (None, Some(action)) => vec![Pattern {
                uri: raw.uri.unwrap_or_else(default::uri),
                path: raw.path,
                method: raw.method,
                header: raw.header,
                query: raw.query,
                remote_ip: raw.remote_ip,
                rewrite: raw.rewrite,
                request: raw.request.unwrap_or_default(),
                response: raw.response.unwrap_or_default(),
                handle_response: raw.handle_response,
                mirror: raw.mirror,
                encode: raw.encode,
                cache: raw.cache.unwrap_or_default(),
                ..Pattern::new(action)
            }],
            _ => {
                return Err(String::from(
                    "missing 'match', 'forward', 'serve' or 'redirect'",
                ));
            }
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
            client_auth: raw.client_auth,
            tls_options: raw.tls_options,
            https_redirect: raw.https_redirect,
            http2: raw.http2,
            trusted_proxies,
            ip_allow: raw.ip_allow,
            ip_deny: raw.ip_deny,
            rate_limit: raw.rate_limit,
            basic_auth: raw.basic_auth,
            forward_auth: raw.forward_auth,
            security_headers: raw.security_headers,
            cors: raw.cors,
            max_connections_per_ip: raw.max_connections_per_ip,
            proxy_protocol: raw.proxy_protocol,
            proxy_protocol_in: raw.proxy_protocol_in,
            match_timeout: raw.match_timeout,
            udp_idle_timeout: raw.udp_idle_timeout,
            read_header_timeout: raw.read_header_timeout,
            idle_timeout: raw.idle_timeout,
            stream_timeout: raw.stream_timeout,
            stream_close_delay: raw.stream_close_delay,
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

    /// Client networks this entry accepts, on top of the server's lists
    /// (http only). A matching request from elsewhere gets `403`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_allow: Vec<IpNet>,

    /// Client networks this entry rejects with `403` (http only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_deny: Vec<IpNet>,

    /// Request rate limit of this entry, on top of the server's (http only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,

    /// HTTP Basic authentication of this entry, replacing the server's (http
    /// only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuth>,

    /// Authentication delegated to an external service, replacing the
    /// server's (http only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forward_auth: Option<ForwardAuth>,

    /// Preset of security response headers, replacing the server's; `off`
    /// turns the server's off for this entry (http only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_headers: Option<SecurityHeaders>,

    /// Cross-origin resource sharing, replacing the server's (http only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Cors>,

    /// What to do when the pattern matches.
    #[serde(flatten)]
    pub action: Action,

    /// New URI of the request before it is forwarded or served, a
    /// [`Template`] starting with `/`. Without a `?` the query is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,

    /// Request customization: method and headers.
    #[serde(default)]
    pub request: RequestConfig,

    /// Response header customization.
    #[serde(default)]
    pub response: ResponseConfig,

    /// Handlers of upstream responses, the first matching one runs
    /// (`forward` only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handle_response: Vec<HandleResponse>,

    /// Pools that get a copy of the requests, their responses dropped
    /// (`forward` only).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub mirror: Vec<Mirror>,

    /// Encodings to compress responses with, in order of preference. Applies
    /// to files and proxied responses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub encode: Vec<Encoding>,

    /// Response cache for this pattern.
    #[serde(default)]
    pub cache: Cache,

    /// TLS server names (SNI), exact or `*.example.com` (tcp only).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub sni: Vec<String>,

    /// ALPN protocols; any may match (tcp only).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub alpn: Vec<String>,

    /// Protocol detected from the first bytes of the connection (tcp only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<L4Protocol>,

    /// `Host` header of an HTTP/1 request, exact or `*.example.com` (tcp only).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub http_host: Vec<String>,
}

impl Pattern {
    /// An entry that matches everything and runs `action`.
    fn new(action: Action) -> Self {
        Self {
            uri: default::uri(),
            host: Vec::new(),
            path: Vec::new(),
            method: Vec::new(),
            header: BTreeMap::new(),
            query: BTreeMap::new(),
            remote_ip: Vec::new(),
            ip_allow: Vec::new(),
            ip_deny: Vec::new(),
            rate_limit: None,
            basic_auth: None,
            forward_auth: None,
            security_headers: None,
            cors: None,
            action,
            rewrite: None,
            request: RequestConfig::default(),
            response: ResponseConfig::default(),
            handle_response: Vec::new(),
            mirror: Vec::new(),
            encode: Vec::new(),
            cache: Cache::default(),
            sni: Vec::new(),
            alpn: Vec::new(),
            protocol: None,
            http_host: Vec::new(),
        }
    }

    /// Checks `rewrite` and `request.method`.
    fn validate_rewrite(&self) -> Result<(), String> {
        if matches!(self.action, Action::Redirect(_))
            && (self.rewrite.is_some() || self.request.method.is_some())
        {
            return Err(String::from(
                "rewrite and request.method do not apply to redirect",
            ));
        }
        if let Some(rewrite) = &self.rewrite {
            let template: Template = rewrite.parse().map_err(|err| format!("rewrite: {err}"))?;
            let sample = template.render("example.com", &http::Uri::from_static("/a?b=c"));
            if !rewrite.starts_with('/')
                || rewrite.contains('#')
                || sample.parse::<http::uri::PathAndQuery>().is_err()
            {
                return Err(format!(
                    "rewrite: {rewrite:?} must be a path starting with /, optionally with a query"
                ));
            }
        }
        if let Some(method) = &self.request.method
            && (method.is_empty() || http::Method::from_bytes(method.as_bytes()).is_err())
        {
            return Err(format!("request.method: {method:?} is not an HTTP method"));
        }
        if self.request.max_body_size == Some(0) {
            return Err(String::from(
                "request.max_body_size must be greater than zero",
            ));
        }
        Ok(())
    }

    /// Returns true if the entry uses a layer 4 matcher (`sni`, `alpn`,
    /// `protocol` or `http_host`).
    #[must_use]
    pub fn has_l4_matchers(&self) -> bool {
        !self.sni.is_empty()
            || !self.alpn.is_empty()
            || self.protocol.is_some()
            || !self.http_host.is_empty()
    }

    /// Returns true if the entry uses an HTTP request matcher.
    #[must_use]
    fn has_http_matchers(&self) -> bool {
        self.uri != "/"
            || !self.host.is_empty()
            || !self.path.is_empty()
            || !self.method.is_empty()
            || !self.header.is_empty()
            || !self.query.is_empty()
            || !self.remote_ip.is_empty()
    }

    /// Returns true if the entry sets `rewrite`, `request`, `response`,
    /// `handle_response`, `mirror`, `encode` or `cache`, which only HTTP
    /// servers use.
    fn has_http_options(&self) -> bool {
        !self.encode.is_empty()
            || !self.mirror.is_empty()
            || self.rewrite.is_some()
            || self.request != RequestConfig::default()
            || self.response != ResponseConfig::default()
            || !self.handle_response.is_empty()
            || self.cache != Cache::default()
    }
}

/// Protocol recognized by the `protocol` matcher of `tcp` servers.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum L4Protocol {
    /// A TLS ClientHello, or any connection to a `tcp` server that terminates TLS.
    Tls,

    /// An HTTP/1 request line, or the HTTP/2 connection preface.
    Http,

    /// An SSH banner (`SSH-`).
    Ssh,
}

/// Content encoding RXH can produce.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    /// Zstandard (`zstd`).
    Zstd,

    /// Brotli (`br`).
    Br,

    /// Gzip (`gzip`).
    Gzip,
}

impl Encoding {
    /// Token used in `Accept-Encoding` and `Content-Encoding`.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Zstd => "zstd",
            Self::Br => "br",
            Self::Gzip => "gzip",
        }
    }
}

/// Settings of a `serve` action: `serve = "dir"` or a table.
///
/// ```toml
/// serve = "/var/www"
/// serve = { root = ".", browse = false, hide = [".git", "*.pem"], precompressed = ["br", "gzip"] }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "FileServerOption")]
pub struct FileServer {
    /// Directory the files come from.
    pub root: String,

    /// List directories that have no index file.
    pub browse: bool,

    /// Index file names tried in order for a directory.
    pub index: Vec<String>,

    /// File names or `*` globs, matched against every path component, that
    /// answer 404 and are left out of listings.
    pub hide: Vec<String>,

    /// Look for `FILE.zst`, `FILE.br` and `FILE.gz` next to a file, in this
    /// order of preference, when the client accepts the encoding.
    pub precompressed: Vec<Encoding>,
}

impl FileServer {
    /// Settings with every default for `root`.
    #[must_use]
    pub fn new(root: impl Into<String>) -> Self {
        Self {
            root: root.into(),
            browse: true,
            index: vec![String::from("index.html")],
            hide: Vec::new(),
            precompressed: Vec::new(),
        }
    }
}

impl FileServer {
    fn validate(&self) -> Result<(), String> {
        for entry in &self.hide {
            if entry.is_empty() || entry.contains('/') {
                return Err(format!(
                    "serve.hide entry {entry:?} must be a file name or a `*` glob without `/`"
                ));
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileServerOption {
    Root(String),
    Table(FileServerTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileServerTable {
    root: String,
    #[serde(default = "default::true_value")]
    browse: bool,
    #[serde(default, deserialize_with = "one_or_many")]
    index: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    hide: Vec<String>,
    #[serde(default)]
    precompressed: Vec<Encoding>,
}

impl From<FileServerOption> for FileServer {
    fn from(option: FileServerOption) -> Self {
        match option {
            FileServerOption::Root(root) => Self::new(root),
            FileServerOption::Table(table) => {
                let mut server = Self::new(table.root);
                server.browse = table.browse;
                if !table.index.is_empty() {
                    server.index = table.index;
                }
                server.hide = table.hide;
                server.precompressed = table.precompressed;
                server
            }
        }
    }
}

/// Action executed when a [`Pattern`] matches.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Proxy to a load balancing group.
    Forward(Forward),

    /// Serve static files from a root directory.
    Serve(FileServer),

    /// Answer with a redirect.
    Redirect(Redirect),
}

/// Settings of a `redirect` action: `redirect = "target"` or a table.
///
/// ```toml
/// redirect = "https://example.com{uri}"
/// redirect = { to = "/new{path}", status = 301 }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(from = "RedirectOption")]
pub struct Redirect {
    /// Value of the `Location` header, a [`Template`].
    pub to: String,

    /// Response status: 301, 302, 303, 307 or 308.
    pub status: u16,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RedirectOption {
    To(String),
    Table(RedirectTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RedirectTable {
    to: String,
    #[serde(default = "default::redirect_status")]
    status: u16,
}

impl From<RedirectOption> for Redirect {
    fn from(option: RedirectOption) -> Self {
        match option {
            RedirectOption::To(to) => Self {
                to,
                status: default::redirect_status(),
            },
            RedirectOption::Table(table) => Self {
                to: table.to,
                status: table.status,
            },
        }
    }
}

impl Redirect {
    fn validate(&self) -> Result<(), String> {
        self.to.parse::<Template>()?;
        self.validate_status()
    }

    fn validate_status(&self) -> Result<(), String> {
        if ![301, 302, 303, 307, 308].contains(&self.status) {
            return Err(format!(
                "status {} is not a redirect, expected 301, 302, 303, 307 or 308",
                self.status
            ));
        }
        Ok(())
    }
}

/// One entry of `handle_response` on a `forward` route: what to do with the
/// upstream responses that match `status` and `header`.
///
/// ```toml
/// handle_response = [
///     { status = 404, replace_status = 200 },
///     { header = { x-accel-redirect = "*" }, serve = "/srv/private", rewrite = "{header.x-accel-redirect}" },
///     { status = [502, 503], serve = "/srv/errors", rewrite = "/{status}.html" },
///     { status = 401, redirect = "/login?next={path}" },
///     { status = "5xx", copy_response = 500 },
/// ]
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct HandleResponse {
    /// Statuses to handle: codes or classes (`"5xx"`), any may match. Empty
    /// means every status.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub status: Vec<StatusMatcher>,

    /// Required response headers. The value `*` only requires presence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,

    /// Send the upstream response with this status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace_status: Option<u16>,

    /// Send the upstream response as it is (`true`) or with a status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_response: Option<CopyResponse>,

    /// Drop the upstream response and answer with a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<FileServer>,

    /// Drop the upstream response and answer with a redirect. The target
    /// may use the response placeholders of [`Template::for_response`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,

    /// Path of the file `serve` looks up instead of the request path, a
    /// [`Template::for_response`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<String>,
}

/// Value of `copy_response`: `true` or the status to send.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(untagged)]
pub enum CopyResponse {
    /// `true` keeps the upstream status; `false` is rejected.
    Enabled(bool),

    /// Status sent instead of the upstream one.
    Status(u16),
}

impl HandleResponse {
    /// Returns true if an upstream response with `status` and `headers` is
    /// handled by this entry.
    #[must_use]
    pub fn matches(&self, status: u16, headers: &http::HeaderMap) -> bool {
        (self.status.is_empty() || self.status.iter().any(|matcher| matcher.matches(status)))
            && self.header.iter().all(|(name, want)| {
                let mut values = headers.get_all(name).iter();
                if want == "*" {
                    values.next().is_some()
                } else {
                    values.any(|value| value.as_bytes() == want.as_bytes())
                }
            })
    }

    /// Status the upstream response is sent with, when the entry replaces it.
    #[must_use]
    pub fn new_status(&self) -> Option<u16> {
        match self.copy_response {
            Some(CopyResponse::Status(status)) => Some(status),
            _ => self.replace_status,
        }
    }

    fn validate(&self) -> Result<(), String> {
        let actions = [
            self.replace_status.is_some(),
            self.copy_response.is_some(),
            self.serve.is_some(),
            self.redirect.is_some(),
        ];
        if actions.iter().filter(|set| **set).count() != 1 {
            return Err(String::from(
                "needs exactly one of replace_status, copy_response, serve or redirect",
            ));
        }
        for name in self.header.keys() {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("invalid header name {name:?}"));
            }
        }
        if self.copy_response == Some(CopyResponse::Enabled(false)) {
            return Err(String::from("copy_response must be true or a status"));
        }
        if let Some(status) = self.new_status()
            && !(200..=599).contains(&status)
        {
            return Err(format!("status {status} must be between 200 and 599"));
        }
        if let Some(files) = &self.serve {
            files.validate()?;
        }
        if let Some(redirect) = &self.redirect {
            Template::for_response(&redirect.to).map_err(|err| format!("redirect: {err}"))?;
            redirect
                .validate_status()
                .map_err(|err| format!("redirect: {err}"))?;
        }
        match &self.rewrite {
            Some(_) if self.serve.is_none() => Err(String::from("rewrite needs serve")),
            Some(rewrite) => Template::for_response(rewrite)
                .map(drop)
                .map_err(|err| format!("rewrite: {err}")),
            None => Ok(()),
        }
    }
}

/// Target of a `forward` action: a named group (`"@api"`) or an inline one.
///
/// ```toml
/// forward = "@api"                                  # named group
/// forward = "127.0.0.1:8080"                        # single backend
/// forward = ["127.0.0.1:8080", "127.0.0.1:8081"]    # list of backends
/// forward = { policy = "ip_hash", backends = ["127.0.0.1:8080"] }
/// forward = [{ pool = "v1", weight = 90 }, { pool = "v2", weight = 10 }]
/// ```
#[derive(Deserialize, Debug, Clone)]
#[serde(from = "ForwardOption")]
pub enum Forward {
    /// Reference to an entry of [`Config::upstreams`].
    Group(String),

    /// Anonymous group declared in place.
    Inline(Box<UpstreamGroup>),

    /// Traffic split between entries of [`Config::upstreams`] by weight,
    /// for canary releases.
    Split(Vec<WeightedPool>),
}

/// One pool of a [`Forward::Split`]: `{ pool = "v2", weight = 10 }`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WeightedPool {
    /// Name of an entry of [`Config::upstreams`], without `@`.
    pub pool: String,

    /// Share of the requests (HTTP) or connections (TCP), relative to the
    /// other pools of the split. Zero sends nothing to the pool.
    #[serde(default = "default::weight")]
    pub weight: u32,
}

/// One entry of `mirror` on a `forward` route: `{ pool = "shadow", percent =
/// 10 }`. A copy of the request goes to a member of the pool and its
/// response is dropped; the client only ever sees the `forward` response.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mirror {
    /// Name of an entry of [`Config::upstreams`], without `@`.
    pub pool: String,

    /// Share of the requests copied, from 0 to 100.
    #[serde(default = "default::mirror_percent")]
    pub percent: u8,

    /// Largest request body copied. Requests with a bigger body, or a body
    /// of unknown length, are forwarded without a copy.
    #[serde(
        default = "default::mirror_max_body_size",
        deserialize_with = "size::deserialize"
    )]
    pub max_body_size: usize,
}

impl Serialize for Forward {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Forward::Group(name) => serializer.serialize_str(&format!("@{name}")),
            Forward::Inline(group) => group.serialize(serializer),
            Forward::Split(split) => split.serialize(serializer),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ForwardOption {
    Text(String),
    List(Vec<Backend>),
    Split(Vec<WeightedPool>),
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
            ForwardOption::Split(split) => Forward::Split(split),
            ForwardOption::Group(group) => Forward::Inline(group),
        }
    }
}

/// Selection policy used when a policy cannot decide, with its own settings.
///
/// ```toml
/// fallback = "random"
/// fallback = { policy = "header", key = "x-user", fallback = "round_robin" }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(from = "FallbackOption")]
pub struct Fallback {
    /// Selection policy.
    pub policy: Policy,

    /// Candidates sampled by `random_choose`.
    pub choose: usize,

    /// Query parameter, header or cookie name for `query`, `header` and `cookie`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// HMAC secret for the `cookie` policy. Cookie values use the secret of
    /// the first `cookie` policy in the chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,

    /// Policy used when this one cannot decide. Defaults to random.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<Fallback>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FallbackOption {
    Text(Policy),
    Table(FallbackTable),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FallbackTable {
    policy: Policy,
    #[serde(default = "default::choose")]
    choose: usize,
    key: Option<String>,
    secret: Option<String>,
    fallback: Option<Box<Fallback>>,
}

impl From<FallbackOption> for Fallback {
    fn from(value: FallbackOption) -> Self {
        match value {
            FallbackOption::Text(policy) => Fallback {
                policy,
                choose: default::choose(),
                key: None,
                secret: None,
                fallback: None,
            },
            FallbackOption::Table(table) => Fallback {
                policy: table.policy,
                choose: table.choose,
                key: table.key,
                secret: table.secret,
                fallback: table.fallback,
            },
        }
    }
}

impl Fallback {
    fn validate(&self) -> Result<(), String> {
        if self.choose == 0 {
            return Err(String::from("fallback: choose must be at least 1"));
        }
        if matches!(self.policy, Policy::Query | Policy::Header) && self.key.is_none() {
            return Err(format!("fallback: policy {} needs a 'key'", self.policy));
        }
        Ok(())
    }
}

/// Which requests are retried on another backend.
///
/// ```toml
/// retry_match = { method = ["GET", "POST"], status = ["502", "5xx"] }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetryMatch {
    /// Methods whose bodyless requests are retried after a failed round trip.
    /// Defaults to `GET`, `HEAD` and `OPTIONS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub method: Vec<String>,

    /// Response statuses that make the proxy discard the response and try
    /// another backend, for retryable requests only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub status: Vec<StatusMatcher>,
}

impl RetryMatch {
    fn validate(&self) -> Result<(), String> {
        for method in &self.method {
            http::Method::from_bytes(method.as_bytes())
                .map_err(|_| format!("retry_match.method: invalid method {method:?}"))?;
        }
        Ok(())
    }

    /// Returns true if a bodyless request with `method` may be retried.
    #[must_use]
    pub fn retries_method(&self, method: &http::Method) -> bool {
        if self.method.is_empty() {
            return matches!(
                *method,
                http::Method::GET | http::Method::HEAD | http::Method::OPTIONS
            );
        }
        self.method
            .iter()
            .any(|name| name.eq_ignore_ascii_case(method.as_str()))
    }

    /// Returns true if a response with `status` should be retried.
    #[must_use]
    pub fn retries_status(&self, status: u16) -> bool {
        self.status.iter().any(|matcher| matcher.matches(status))
    }
}

impl UpstreamGroup {
    /// Secret of the first `cookie` policy of the chain (the group's policy,
    /// then its fallbacks), if any: backends carry cookie values only then.
    #[must_use]
    pub fn cookie_secret(&self) -> Option<&str> {
        if self.policy == Policy::Cookie {
            return Some(self.secret.as_deref().unwrap_or(""));
        }
        let mut node = self.fallback.as_ref();
        while let Some(fallback) = node {
            if fallback.policy == Policy::Cookie {
                return Some(fallback.secret.as_deref().unwrap_or(""));
            }
            node = fallback.fallback.as_deref();
        }
        None
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
    /// A policy name, or a table that can nest another fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Fallback>,

    /// Which requests and responses are retried on another backend
    /// (Caddy `lb_retry_match`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_match: Option<RetryMatch>,

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

    /// Time from sending a request to receiving the response headers
    /// before the client gets `504` (Caddy `response_header_timeout`).
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub response_header_timeout: Option<Duration>,

    /// An upstream read without progress for this long fails the connection.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub read_timeout: Option<Duration>,

    /// An upstream write without progress for this long fails the connection.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub write_timeout: Option<Duration>,

    /// Speak cleartext HTTP/2 to the backends instead of HTTP/1.1.
    #[serde(default)]
    pub h2c: bool,

    /// HTTP/2 connections kept open to each backend with `h2c`; requests
    /// are spread over them in turn.
    #[serde(default = "default::http2_conns_per_host")]
    pub http2_conns_per_host: usize,

    /// How long idle upstream HTTP connections stay in the pool.
    #[serde(default = "default::keepalive", with = "duration")]
    pub keepalive: Duration,

    /// Maximum idle upstream HTTP connections kept per backend.
    #[serde(default = "default::keepalive_idle_conns_per_host")]
    pub keepalive_idle_conns_per_host: usize,

    /// Maximum upstream HTTP/1.1 connections per backend; requests over it
    /// wait for a free one. `0` means no limit (Caddy `max_conns_per_host`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_conns_per_host: usize,

    /// Maximum requests in flight (connections, for `tcp` and `udp` servers)
    /// per backend. `0` means no limit (HAProxy `maxconn`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_connections: usize,

    /// Requests that may wait for a backend when all of them are at
    /// `max_connections`; the ones over it are refused. `0` refuses at once
    /// (HAProxy `maxqueue`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub queue_size: usize,

    /// Longest wait in the queue (HAProxy `timeout queue`).
    #[serde(default = "default::queue_timeout", with = "duration")]
    pub queue_timeout: Duration,

    /// Bytes of each request body read before the request is sent upstream.
    /// `0` streams it (Caddy `request_buffers`).
    #[serde(
        default,
        deserialize_with = "size::deserialize",
        skip_serializing_if = "is_zero"
    )]
    pub request_buffers: usize,

    /// Bytes of each response body read before the response is sent to the
    /// client. `0` streams it (Caddy `response_buffers`).
    #[serde(
        default,
        deserialize_with = "size::deserialize",
        skip_serializing_if = "is_zero"
    )]
    pub response_buffers: usize,

    /// Responses with a `Content-Length` are written to the client in
    /// batches collected for this long. Unset writes every chunk at once
    /// (Caddy `flush_interval -1`).
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub flush_interval: Option<Duration>,

    /// Backends. Port ranges such as `10.0.0.1:8001-8004` expand to many.
    #[serde(default, alias = "to")]
    pub backends: Vec<Backend>,

    /// Active health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<ActiveHealth>,

    /// Passive health checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passive: Option<PassiveHealth>,

    /// Circuit breaker on the error ratio or the latency of each backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker: Option<CircuitBreaker>,

    /// Time over which a backend added at runtime or back from being
    /// unhealthy ramps up from no share of the traffic to its full share.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub slow_start: Option<Duration>,

    /// How the hash policies map a key to a backend.
    #[serde(default)]
    pub hash_type: HashType,

    /// Bounded loads for the hash policies, in percent of the average load
    /// of the available backends (HAProxy `hash-balance-factor`): a backend
    /// at the bound passes its keys on to the next backend of the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash_balance_factor: Option<u32>,

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
            retry_match: None,
            retries: 0,
            try_duration: None,
            try_interval: default::try_interval(),
            dial_timeout: default::dial_timeout(),
            response_header_timeout: None,
            read_timeout: None,
            write_timeout: None,
            h2c: false,
            http2_conns_per_host: default::http2_conns_per_host(),
            keepalive: default::keepalive(),
            keepalive_idle_conns_per_host: default::keepalive_idle_conns_per_host(),
            max_conns_per_host: 0,
            max_connections: 0,
            queue_size: 0,
            queue_timeout: default::queue_timeout(),
            request_buffers: 0,
            response_buffers: 0,
            flush_interval: None,
            backends,
            health: None,
            passive: None,
            circuit_breaker: None,
            slow_start: None,
            hash_type: HashType::default(),
            hash_balance_factor: None,
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
        if self.http2_conns_per_host == 0 {
            return Err(String::from("http2_conns_per_host must be at least 1"));
        }

        for (name, value) in [
            ("response_header_timeout", self.response_header_timeout),
            ("read_timeout", self.read_timeout),
            ("write_timeout", self.write_timeout),
            ("flush_interval", self.flush_interval),
        ] {
            if value.is_some_and(|value| value.is_zero()) {
                return Err(format!("{name} must be greater than zero"));
            }
        }

        if matches!(self.policy, Policy::Query | Policy::Header) && self.key.is_none() {
            return Err(format!("policy {} needs a 'key'", self.policy));
        }

        let mut fallback = self.fallback.as_ref();
        while let Some(node) = fallback {
            node.validate()?;
            fallback = node.fallback.as_deref();
        }

        if let Some(retry_match) = &self.retry_match {
            retry_match.validate()?;
        }

        if let Some(passive) = &self.passive
            && passive.max_fails == 0
        {
            return Err(String::from("passive.max_fails must be at least 1"));
        }

        if let Some(breaker) = &self.circuit_breaker {
            breaker.validate()?;
        }

        if self.queue_size > 0 && self.max_connections == 0 {
            return Err(String::from("queue_size needs max_connections"));
        }
        if self.queue_timeout.is_zero() {
            return Err(String::from("queue_timeout must be above 0"));
        }

        if self.slow_start.is_some_and(|duration| duration.is_zero()) {
            return Err(String::from("slow_start must be above 0"));
        }

        if self.hash_balance_factor.is_some_and(|factor| factor < 100) {
            return Err(String::from(
                "hash_balance_factor must be at least 100 (percent of the average load)",
            ));
        }

        if let Some(health) = &self.health
            && (health.passes == 0 || health.fails == 0)
        {
            return Err(String::from(
                "health.passes and health.fails must be at least 1",
            ));
        }

        if let Some(hooks) = self
            .health
            .as_ref()
            .and_then(|health| health.hooks.as_ref())
        {
            hooks.validate()?;
        }

        if let Some(body) = self
            .health
            .as_ref()
            .and_then(|health| health.body.as_deref())
        {
            regex::Regex::new(body).map_err(|err| format!("health.body: {err}"))?;
        }

        let mut seen = HashSet::new();
        for backend in &backends {
            if !seen.insert(backend.address.as_str()) {
                return Err(format!("backend {} is listed twice", backend.address));
            }
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

/// Algorithm mapping the key of a hash policy to a backend.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "snake_case")]
pub enum HashType {
    /// Highest random weight hashing, computed over the members on each pick.
    #[default]
    Rendezvous,

    /// Maglev lookup table, rebuilt when the members change.
    Maglev,
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

    /// Command and webhook run when a backend turns healthy or unhealthy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HealthHooks>,
}

/// Actions run on `healthy` and `unhealthy` events of active health checks,
/// besides the log line. Each event runs the command, then calls the webhook.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HealthHooks {
    /// Program and arguments, run without a shell. The event is passed in
    /// `RXH_EVENT`, `RXH_POOL`, `RXH_UPSTREAM` and `RXH_REASON`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,

    /// `http://` or `https://` URL that receives each event as a JSON `POST`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
}

impl HealthHooks {
    fn validate(&self) -> Result<(), String> {
        if self.command.is_empty() && self.webhook.is_none() {
            return Err(String::from(
                "health.hooks needs a 'command', a 'webhook' or both",
            ));
        }
        if self.command.first().is_some_and(String::is_empty) {
            return Err(String::from(
                "health.hooks.command: program must not be empty",
            ));
        }
        if let Some(webhook) = &self.webhook {
            let uri: http::Uri = webhook
                .parse()
                .map_err(|err| format!("health.hooks.webhook {webhook:?}: {err}"))?;
            let scheme_ok = matches!(uri.scheme_str(), Some("http" | "https"));
            if !scheme_ok || uri.host().is_none_or(str::is_empty) {
                return Err(format!(
                    "health.hooks.webhook must be an http:// or https:// URL, got {webhook:?}"
                ));
            }
        }
        Ok(())
    }
}

/// Circuit breaker of each backend of a pool. A backend whose share of
/// failed or slow requests in a `window` reaches a ratio is taken out of
/// rotation (open) for `open_duration`, then gets one trial request (half
/// open) that closes the breaker again or reopens it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CircuitBreaker {
    /// Share of failed requests, `0.5` for half of them, that opens the
    /// breaker. Failed dials, broken responses and `error_status` count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_ratio: Option<f64>,

    /// Response statuses counted as errors.
    #[serde(default = "default::breaker_error_status")]
    pub error_status: Vec<StatusMatcher>,

    /// Responses slower than this (time to headers) count as slow.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub latency: Option<Duration>,

    /// Share of slow responses that opens the breaker. `0.5` trips on the
    /// median latency, `0.05` on the 95th percentile.
    #[serde(default = "default::breaker_latency_ratio")]
    pub latency_ratio: f64,

    /// Period over which requests are counted.
    #[serde(default = "default::breaker_duration", with = "duration")]
    pub window: Duration,

    /// Requests in a window below which the breaker never opens.
    #[serde(default = "default::breaker_min_requests")]
    pub min_requests: u32,

    /// How long an open breaker keeps its backend out of rotation.
    #[serde(default = "default::breaker_duration", with = "duration")]
    pub open_duration: Duration,
}

impl CircuitBreaker {
    fn validate(&self) -> Result<(), String> {
        let is_ratio = |ratio: f64| ratio > 0.0 && ratio <= 1.0;
        if self.error_ratio.is_none() && self.latency.is_none() {
            return Err(String::from(
                "circuit_breaker needs 'error_ratio' or 'latency'",
            ));
        }
        if !self.error_ratio.is_none_or(is_ratio) {
            return Err(String::from(
                "circuit_breaker.error_ratio must be above 0 and at most 1",
            ));
        }
        if !is_ratio(self.latency_ratio) {
            return Err(String::from(
                "circuit_breaker.latency_ratio must be above 0 and at most 1",
            ));
        }
        if self.latency.is_some_and(|latency| latency.is_zero()) {
            return Err(String::from("circuit_breaker.latency must be above 0"));
        }
        if self.window.is_zero() || self.open_duration.is_zero() {
            return Err(String::from(
                "circuit_breaker.window and circuit_breaker.open_duration must be above 0",
            ));
        }
        if self.min_requests == 0 {
            return Err(String::from(
                "circuit_breaker.min_requests must be at least 1",
            ));
        }
        Ok(())
    }
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

/// Certificate source of a server that terminates TLS.
///
/// ```toml
/// tls = "auto"                                    # ACME, for every name in `host`
/// tls = { cert = "cert.pem", key = "key.pem" }    # files
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// Certificates obtained and renewed with ACME (HTTP-01, TLS-ALPN-01 or
    /// DNS-01) for every name of the server's `host` list, see [`Acme`].
    Auto,

    /// A PEM certificate chain and its PEM private key, read from files.
    Files {
        /// Path to a PEM encoded certificate chain.
        cert: String,

        /// Path to a PEM encoded private key.
        key: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TlsFiles {
    cert: String,
    key: String,
}

impl Serialize for Tls {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Tls::Auto => serializer.serialize_str("auto"),
            Tls::Files { cert, key } => TlsFiles {
                cert: cert.clone(),
                key: key.clone(),
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Tls {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Tls;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("\"auto\" or a table { cert, key }")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Tls, E> {
                match value {
                    "auto" => Ok(Tls::Auto),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(value), &self)),
                }
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Tls, A::Error> {
                // Keeps the `deny_unknown_fields` errors of the table form.
                let files =
                    TlsFiles::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(Tls::Files {
                    cert: files.cert,
                    key: files.key,
                })
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// What a TLS server asks of client certificates, named after Traefik's
/// `clientAuthType` values.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuthMode {
    /// Asks for a certificate; accepts clients without one and does not
    /// verify the one given.
    Request,

    /// Requires a certificate, without verifying it against `ca`.
    Require,

    /// Accepts clients without a certificate; a certificate that is given
    /// must be signed by one of `ca`.
    VerifyIfGiven,

    /// Requires a certificate signed by one of `ca`.
    #[default]
    RequireAndVerify,
}

impl ClientAuthMode {
    /// True when certificates are verified against `ca`.
    #[must_use]
    pub fn verifies(self) -> bool {
        matches!(self, Self::VerifyIfGiven | Self::RequireAndVerify)
    }

    /// True when clients without a certificate are refused.
    #[must_use]
    pub fn is_mandatory(self) -> bool {
        matches!(self, Self::Require | Self::RequireAndVerify)
    }
}

/// The `client_auth` table of a TLS server: mutual TLS.
///
/// ```toml
/// client_auth = { ca = "clients-ca.pem", header = "X-Client-Subject" }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientAuth {
    /// What is asked of the client.
    #[serde(default)]
    pub mode: ClientAuthMode,

    /// Paths to PEM files with the certificate authorities that sign client
    /// certificates. Needed by the verifying modes, rejected by the others.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ca: Vec<String>,

    /// Request header that carries the subject of the verified client
    /// certificate to the backends (`http` servers, verifying modes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

impl ClientAuth {
    fn validate(&self) -> Result<(), String> {
        const VERIFYING: &str = "\"verify_if_given\" or \"require_and_verify\"";
        if self.mode.verifies() {
            if self.ca.is_empty() {
                return Err(String::from("this mode needs ca"));
            }
        } else if !self.ca.is_empty() {
            return Err(format!(
                "this mode does not verify certificates, remove ca or use {VERIFYING}"
            ));
        } else if self.header.is_some() {
            // The subject of a certificate nobody verified is whatever the
            // client wants it to be.
            return Err(format!(
                "header needs a mode that verifies certificates: {VERIFYING}"
            ));
        }
        if let Some(header) = &self.header
            && http::HeaderName::from_bytes(header.as_bytes()).is_err()
        {
            return Err(format!("invalid header name {header:?}"));
        }
        Ok(())
    }
}

/// Lowest TLS version a server accepts.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsVersion {
    /// TLS 1.2 and 1.3.
    #[default]
    #[serde(rename = "1.2")]
    V1_2,

    /// TLS 1.3 only.
    #[serde(rename = "1.3")]
    V1_3,
}

/// The `tls_options` table of a TLS server: what its handshakes accept.
///
/// ```toml
/// tls_options = { min_version = "1.3", curves = ["X25519", "secp384r1"] }
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsOptions {
    /// Lowest protocol version accepted.
    #[serde(default)]
    pub min_version: TlsVersion,

    /// Cipher suites offered, from [`TlsOptions::CIPHER_SUITES`]. Empty
    /// means all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cipher_suites: Vec<String>,

    /// Key exchange groups offered, from [`TlsOptions::CURVES`] in any
    /// case. Empty means all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub curves: Vec<String>,

    /// Fetches OCSP responses for the certificates and staples them to the
    /// handshakes.
    #[serde(default = "default::true_value")]
    pub ocsp_stapling: bool,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self {
            min_version: TlsVersion::default(),
            cipher_suites: Vec::new(),
            curves: Vec::new(),
            ocsp_stapling: true,
        }
    }
}

impl TlsOptions {
    /// Cipher suites of `cipher_suites`, by their IANA names. The first
    /// three are the TLS 1.3 suites.
    pub const CIPHER_SUITES: [&str; 9] = [
        "TLS_AES_256_GCM_SHA384",
        "TLS_AES_128_GCM_SHA256",
        "TLS_CHACHA20_POLY1305_SHA256",
        "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
        "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
        "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
        "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384",
        "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256",
        "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
    ];

    /// Key exchange groups of `curves`, by their IANA names.
    pub const CURVES: [&str; 4] = ["X25519MLKEM768", "X25519", "secp256r1", "secp384r1"];

    fn validate(&self) -> Result<(), String> {
        for suite in &self.cipher_suites {
            if !Self::CIPHER_SUITES.contains(&suite.as_str()) {
                return Err(format!(
                    "unknown cipher suite {suite:?}, expected one of {}",
                    Self::CIPHER_SUITES.join(", ")
                ));
            }
        }
        for curve in &self.curves {
            if !Self::CURVES
                .iter()
                .any(|known| known.eq_ignore_ascii_case(curve))
            {
                return Err(format!(
                    "unknown curve {curve:?}, expected one of {}",
                    Self::CURVES.join(", ")
                ));
            }
        }
        // TLS 1.2 suites are the ones named after their key exchange.
        let tls13 = |suite: &String| !suite.contains("_WITH_");
        if self.min_version == TlsVersion::V1_3
            && !self.cipher_suites.is_empty()
            && !self.cipher_suites.iter().any(tls13)
        {
            return Err(String::from(
                "min_version \"1.3\" needs a TLS 1.3 cipher suite in cipher_suites",
            ));
        }
        Ok(())
    }
}

/// The `[acme]` table: where `tls = "auto"` certificates come from and where
/// they are kept.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Acme {
    /// Contact address registered with the account, for expiry notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Directory URL of the certificate authority.
    #[serde(default = "default::acme_ca")]
    pub ca: String,

    /// PEM file with the root certificate trusted for the CA's own HTTPS
    /// endpoint instead of the system roots, for test CAs like Pebble.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_root: Option<String>,

    /// Directory of the account and the certificates. A relative path is
    /// relative to the directory of the main configuration file.
    #[serde(default = "default::acme_storage")]
    pub storage: String,

    /// Certificates expiring within this time are renewed.
    #[serde(default = "default::acme_renew_before", with = "duration")]
    pub renew_before: Duration,

    /// Port the CA connects to for HTTP-01 challenges.
    #[serde(default = "default::acme_http_port")]
    pub http_port: u16,

    /// How the CA validates the names.
    #[serde(default)]
    pub challenge: AcmeChallenge,

    /// The `[acme.dns]` table: the DNS provider of `challenge = "dns-01"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dns: Option<AcmeDns>,
}

/// The `challenge` key of `[acme]`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AcmeChallenge {
    /// The CA fetches a token over plain HTTP on port 80 (`http_port` here).
    #[default]
    #[serde(rename = "http-01")]
    Http01,

    /// The CA makes a TLS handshake with ALPN `acme-tls/1` on port 443, which
    /// must reach the listener of the `tls = "auto"` server. Needs no plain
    /// HTTP port.
    #[serde(rename = "tls-alpn-01")]
    TlsAlpn01,

    /// The CA looks up a TXT record at `_acme-challenge.NAME`, which rxh
    /// creates through the API of the DNS provider in [`AcmeDns`]. The only
    /// challenge that certifies wildcards; needs no open port.
    #[serde(rename = "dns-01")]
    Dns01,
}

/// The `provider` key of `[acme.dns]`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DnsProvider {
    /// Cloudflare API, with `api_token`.
    Cloudflare,

    /// Amazon Route 53, with `access_key_id` and `secret_access_key`.
    Route53,

    /// DNS UPDATE (RFC 2136) sent to `server`, signed with TSIG if `tsig_key`
    /// and `tsig_secret` are set.
    Rfc2136,
}

impl fmt::Display for DnsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cloudflare => "cloudflare",
            Self::Route53 => "route53",
            Self::Rfc2136 => "rfc2136",
        })
    }
}

/// The `[acme.dns]` table: where DNS-01 challenge records are created.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcmeDns {
    /// Who hosts the zones of the certified names.
    pub provider: DnsProvider,

    /// Cloudflare API token with the `Zone:Read` and `DNS:Edit` permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<String>,

    /// Route 53: access key of an IAM user allowed to list hosted zones and
    /// change record sets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,

    /// Route 53: secret of `access_key_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,

    /// Cloudflare and Route 53: identifier of the zone, instead of looking
    /// it up by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,

    /// Cloudflare and Route 53: base URL of the API instead of the public
    /// one, for tests and compatible services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,

    /// RFC 2136: primary name server of the zone, `host` or `host:port`
    /// (port 53 by default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,

    /// RFC 2136: name of the TSIG key (HMAC-SHA256) that signs the updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tsig_key: Option<String>,

    /// RFC 2136: base64 secret of `tsig_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tsig_secret: Option<String>,

    /// Time between creating the record and asking the CA to validate, for
    /// the record to reach every name server of the zone.
    #[serde(default = "default::acme_dns_propagation_wait", with = "duration")]
    pub propagation_wait: Duration,
}

impl AcmeDns {
    /// Replaces the secrets with `***`, for output that leaves the process.
    pub fn redact(&mut self) {
        let secrets = [
            &mut self.api_token,
            &mut self.secret_access_key,
            &mut self.tsig_secret,
        ];
        let present = secrets.into_iter().flatten();
        for secret in present {
            *secret = String::from("***");
        }
    }

    /// Puts back the secrets of `running` where this table has the `***`
    /// of [`AcmeDns::redact`], so a redacted copy can be loaded again.
    pub fn restore_redacted(&mut self, running: &Self) {
        let secrets = [
            (&mut self.api_token, &running.api_token),
            (&mut self.secret_access_key, &running.secret_access_key),
            (&mut self.tsig_secret, &running.tsig_secret),
        ];
        for (secret, running) in secrets {
            if secret.as_deref() == Some("***") {
                secret.clone_from(running);
            }
        }
    }

    /// The decoded `tsig_secret`, if there is one.
    ///
    /// # Errors
    ///
    /// Fails if the secret is not base64.
    pub fn tsig_secret_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        self.tsig_secret
            .as_deref()
            .map(|secret| decode_base64(secret).ok_or_else(|| String::from("is not base64")))
            .transpose()
    }

    fn validate(&self) -> Result<(), String> {
        use DnsProvider::*;

        let provider = self.provider;
        let keys: [(
            &str,
            Option<&String>,
            &[DnsProvider],
            /* required= */ bool,
        ); 8] = [
            ("api_token", self.api_token.as_ref(), &[Cloudflare], true),
            (
                "access_key_id",
                self.access_key_id.as_ref(),
                &[Route53],
                true,
            ),
            (
                "secret_access_key",
                self.secret_access_key.as_ref(),
                &[Route53],
                true,
            ),
            (
                "zone_id",
                self.zone_id.as_ref(),
                &[Cloudflare, Route53],
                false,
            ),
            (
                "api_url",
                self.api_url.as_ref(),
                &[Cloudflare, Route53],
                false,
            ),
            ("server", self.server.as_ref(), &[Rfc2136], true),
            ("tsig_key", self.tsig_key.as_ref(), &[Rfc2136], false),
            ("tsig_secret", self.tsig_secret.as_ref(), &[Rfc2136], false),
        ];
        for (key, value, providers, required) in keys {
            let used = providers.contains(&provider);
            match value {
                Some(_) if !used => {
                    return Err(format!("{key} is not used by provider {provider}"));
                }
                Some(value) if value.trim().is_empty() => {
                    return Err(format!("{key} must not be empty"));
                }
                None if used && required => {
                    return Err(format!("provider {provider} needs {key}"));
                }
                _ => {}
            }
        }
        if let Some(url) = &self.api_url
            && !url.starts_with("https://")
            && !url.starts_with("http://")
        {
            return Err(format!("api_url {url:?} must be an http(s):// URL"));
        }
        if self.tsig_key.is_some() != self.tsig_secret.is_some() {
            return Err(String::from("tsig_key and tsig_secret go together"));
        }
        self.tsig_secret_bytes()
            .map_err(|err| format!("tsig_secret {err}"))?;
        Ok(())
    }
}

/// Decodes standard base64, with or without padding.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in text.trim_end_matches('=').bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
        }
    }
    // A lone leftover character carries no whole byte.
    (text.trim_end_matches('=').len() % 4 != 1).then_some(bytes)
}

impl Default for Acme {
    fn default() -> Self {
        Self {
            email: None,
            ca: default::acme_ca(),
            ca_root: None,
            storage: default::acme_storage(),
            renew_before: default::acme_renew_before(),
            http_port: default::acme_http_port(),
            challenge: AcmeChallenge::default(),
            dns: None,
        }
    }
}

impl Acme {
    fn validate(&self) -> Result<(), String> {
        if !self.ca.starts_with("https://") {
            return Err(format!("ca {:?} must be an https:// URL", self.ca));
        }
        if let Some(email) = &self.email
            && (!email.contains('@') || email.contains(char::is_whitespace))
        {
            return Err(format!("email {email:?} is not an e-mail address"));
        }
        if self.storage.trim().is_empty() {
            return Err(String::from("storage must not be empty"));
        }
        if self.renew_before.is_zero() {
            return Err(String::from("renew_before must be greater than zero"));
        }
        if self.http_port == 0 {
            return Err(String::from("http_port must not be 0"));
        }
        match &self.dns {
            Some(dns) => dns.validate().map_err(|err| format!("dns: {err}"))?,
            None if self.challenge == AcmeChallenge::Dns01 => {
                return Err(String::from(
                    "challenge = \"dns-01\" needs the DNS provider in [acme.dns]",
                ));
            }
            None => {}
        }
        Ok(())
    }
}

/// Checks that `name` is a DNS name a public CA can certify: ASCII letters,
/// digits and hyphens in dot separated labels, no wildcard, no IP address.
/// Also what makes the name safe as a directory name in the ACME storage.
///
/// # Errors
///
/// Returns a message naming the problem.
pub fn validate_domain(name: &str) -> Result<(), String> {
    let invalid = |why: &str| Err(format!("{name:?} {why}"));
    if name.starts_with("*.") {
        return invalid(
            "is a wildcard, which only [acme] challenge = \"dns-01\" can certify \
             (one label deep, like *.example.com)",
        );
    }
    if name.is_empty() || name.len() > 253 {
        return invalid("is not a domain name (1 to 253 characters)");
    }
    let labels: Vec<&str> = name.split('.').collect();
    if labels.len() < 2 {
        return invalid("is not a fully qualified domain name");
    }
    for label in &labels {
        let valid = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !valid {
            return invalid("is not a domain name (letters, digits and hyphens per label)");
        }
    }
    if labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return invalid("is an IP address, not a domain name");
    }
    Ok(())
}

/// Per-pattern request customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequestConfig {
    /// Method the request is forwarded or served with instead of its own,
    /// upper cased.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Largest request body accepted; bigger ones get `413`.
    #[serde(
        default,
        deserialize_with = "size::deserialize_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_body_size: Option<usize>,

    /// Header rules.
    #[serde(default)]
    pub headers: RequestHeaders,
}

/// Per-pattern response customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    /// Header rules.
    #[serde(default)]
    pub headers: ResponseHeaders,
}

/// Request header rules, applied before the request goes upstream.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// `Forwarded` header (RFC 7239) behavior.
    #[serde(default)]
    pub forwarded: ForwardedHeader,

    /// Headers to set, replacing existing values.
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Headers to append a value to, keeping existing values.
    #[serde(default)]
    pub add: HashMap<String, String>,

    /// Headers to remove. A trailing `*` removes every header with that prefix.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Regex rewrites of header values.
    #[serde(default)]
    pub replace: HashMap<String, HeaderReplace>,
}

/// Regex rewrite of every value of a header.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HeaderReplace {
    /// Regular expression to look for.
    pub search: String,

    /// Replacement, `$1` and `${name}` refer to capture groups.
    pub replace: String,
}

fn validate_header_rules(
    set: &HashMap<String, String>,
    add: &HashMap<String, String>,
    remove: &[String],
    replace: &HashMap<String, HeaderReplace>,
) -> Result<(), String> {
    let names = set.keys().chain(add.keys()).chain(replace.keys());
    for name in names {
        http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
    }
    for (name, rule) in replace {
        regex::Regex::new(&rule.search)
            .map_err(|err| format!("replace.{name}: invalid regex: {err}"))?;
    }
    for name in remove {
        if name == "*" {
            return Err(String::from(
                "removing every header with \"*\" is not allowed",
            ));
        }
        let name = name.strip_suffix('*').unwrap_or(name);
        http::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("invalid header name {name:?}"))?;
    }
    Ok(())
}

impl RequestHeaders {
    /// Checks header names and wildcards.
    pub fn validate(&self) -> Result<(), String> {
        validate_header_rules(&self.set, &self.add, &self.remove, &self.replace)
    }
}

impl ResponseHeaders {
    /// Checks header names and wildcards.
    pub fn validate(&self) -> Result<(), String> {
        validate_header_rules(&self.set, &self.add, &self.remove, &self.replace)
    }
}

/// Response header rules, applied before the response goes to the client.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
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

    /// Headers to append a value to, keeping existing values.
    #[serde(default)]
    pub add: HashMap<String, String>,

    /// Headers to remove. A trailing `*` removes every header with that prefix.
    #[serde(default)]
    pub remove: Vec<String>,

    /// Regex rewrites of header values.
    #[serde(default)]
    pub replace: HashMap<String, HeaderReplace>,
}

/// `Forwarded` header settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ViaHeader {
    /// Appends this proxy to the `Via` header of responses.
    #[serde(default)]
    pub extend: bool,
}

/// `Server` response header settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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

/// Token bucket rate limit of requests: `rate` requests every `per`, with
/// bursts of up to `burst` requests, counted separately for each `key`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Requests allowed every `per`.
    pub rate: u32,

    /// Period of `rate`.
    #[serde(default = "default::rate_limit_per", with = "duration")]
    pub per: Duration,

    /// Requests a client can send at once before `rate` applies. Defaults to
    /// `rate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<u32>,

    /// What identifies a client.
    #[serde(default)]
    pub key: RateLimitKey,
}

impl RateLimit {
    /// Size of the bucket: `burst`, or `rate` when unset.
    #[must_use]
    pub fn burst(&self) -> u32 {
        self.burst.unwrap_or(self.rate)
    }

    fn validate(&self) -> Result<(), String> {
        if self.rate == 0 {
            return Err(String::from("rate must be greater than zero"));
        }
        if self.per.is_zero() {
            return Err(String::from("per must be greater than zero"));
        }
        if self.burst == Some(0) {
            return Err(String::from("burst must be greater than zero"));
        }
        Ok(())
    }
}

/// What a [`RateLimit`] counts requests by.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
#[serde(try_from = "String", into = "String")]
pub enum RateLimitKey {
    /// The client IP: `X-Forwarded-For` of `trusted_proxies`, else the peer.
    #[default]
    ClientIp,

    /// The value of a request header, such as an API key. Requests without
    /// the header are counted by client IP.
    Header(String),
}

impl TryFrom<String> for RateLimitKey {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        match text.strip_prefix("header:") {
            _ if text == "client_ip" => Ok(Self::ClientIp),
            Some(name) if http::HeaderName::from_bytes(name.as_bytes()).is_ok() => {
                Ok(Self::Header(name.to_owned()))
            }
            Some(name) => Err(format!("invalid header name {name:?} in rate limit key")),
            None => Err(format!(
                "invalid rate limit key {text:?}, expected \"client_ip\" or \"header:<name>\""
            )),
        }
    }
}

impl From<RateLimitKey> for String {
    fn from(key: RateLimitKey) -> Self {
        match key {
            RateLimitKey::ClientIp => Self::from("client_ip"),
            RateLimitKey::Header(name) => format!("header:{name}"),
        }
    }
}

/// Checks the `basic_auth` and `forward_auth` of a server or match entry.
fn validate_auth(
    protocol: Protocol,
    basic_auth: Option<&BasicAuth>,
    forward_auth: Option<&ForwardAuth>,
) -> Result<(), String> {
    if let Some(basic_auth) = basic_auth {
        if protocol != Protocol::Http {
            return Err(String::from("basic_auth only applies to http servers"));
        }
        basic_auth
            .validate()
            .map_err(|err| format!("basic_auth: {err}"))?;
    }
    if let Some(forward_auth) = forward_auth {
        if protocol != Protocol::Http {
            return Err(String::from("forward_auth only applies to http servers"));
        }
        forward_auth
            .validate()
            .map_err(|err| format!("forward_auth: {err}"))?;
    }
    Ok(())
}

/// Checks the `security_headers` and `cors` of a server or match entry.
fn validate_response_policy(
    protocol: Protocol,
    security_headers: Option<SecurityHeaders>,
    cors: Option<&Cors>,
) -> Result<(), String> {
    if security_headers.is_some() && protocol != Protocol::Http {
        return Err(String::from(
            "security_headers only applies to http servers",
        ));
    }
    if let Some(cors) = cors {
        if protocol != Protocol::Http {
            return Err(String::from("cors only applies to http servers"));
        }
        cors.validate().map_err(|err| format!("cors: {err}"))?;
    }
    Ok(())
}

/// Preset of security response headers. Each header is only added when the
/// response does not already have it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecurityHeaders {
    /// No headers; in a match entry, turns the server's preset off.
    Off,

    /// Headers that break no site: `nosniff`, `SAMEORIGIN` framing,
    /// `strict-origin-when-cross-origin` referrers and one year of HSTS.
    Basic,

    /// `basic` hardened: no framing, no referrers, two years of HSTS with
    /// subdomains, a same-origin CSP, COOP and a restrictive
    /// `Permissions-Policy`.
    Strict,
}

impl SecurityHeaders {
    /// Headers of the preset, as `(name, value)` pairs. HSTS is left out
    /// when `tls` is false, browsers ignore it over plain HTTP.
    #[must_use]
    pub fn headers(self, tls: bool) -> Vec<(&'static str, &'static str)> {
        let mut headers = match self {
            Self::Off => return Vec::new(),
            Self::Basic => vec![
                ("x-content-type-options", "nosniff"),
                ("x-frame-options", "SAMEORIGIN"),
                ("referrer-policy", "strict-origin-when-cross-origin"),
            ],
            Self::Strict => vec![
                ("x-content-type-options", "nosniff"),
                ("x-frame-options", "DENY"),
                ("referrer-policy", "no-referrer"),
                (
                    "content-security-policy",
                    "default-src 'self'; base-uri 'self'; form-action 'self'; \
                     frame-ancestors 'none'; object-src 'none'",
                ),
                ("cross-origin-opener-policy", "same-origin"),
                (
                    "permissions-policy",
                    "camera=(), microphone=(), geolocation=(), payment=(), usb=()",
                ),
            ],
        };
        if tls {
            headers.push((
                "strict-transport-security",
                match self {
                    Self::Strict => "max-age=63072000; includeSubDomains",
                    _ => "max-age=31536000",
                },
            ));
        }
        headers
    }
}

/// Cross-origin resource sharing: preflight answers and `Access-Control-*`
/// response headers for the allowed origins.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Cors {
    /// Allowed origins: `https://app.example.com`, `https://*.example.com`
    /// for any subdomain, or `*` for any origin.
    #[serde(deserialize_with = "one_or_many")]
    pub origins: Vec<String>,

    /// Methods allowed in preflight answers.
    #[serde(default = "default::cors_methods", deserialize_with = "one_or_many")]
    pub methods: Vec<String>,

    /// Request headers allowed in preflight answers. Empty allows the ones
    /// the preflight asks for.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub headers: Vec<String>,

    /// Response headers scripts may read besides the safelisted ones.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub expose_headers: Vec<String>,

    /// Allows cookies and `Authorization` on cross-origin requests.
    #[serde(default)]
    pub credentials: bool,

    /// Time browsers may cache a preflight answer.
    #[serde(
        default,
        with = "duration::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_age: Option<Duration>,
}

impl Cors {
    fn validate(&self) -> Result<(), String> {
        if self.origins.is_empty() {
            return Err(String::from("origins must not be empty"));
        }
        for origin in &self.origins {
            if origin == "*" {
                if self.credentials {
                    return Err(String::from(
                        "origin \"*\" cannot be combined with credentials = true, list the \
                         origins",
                    ));
                }
                continue;
            }
            let valid = origin.split_once("://").is_some_and(|(scheme, host)| {
                matches!(scheme, "http" | "https")
                    && !host.is_empty()
                    && host == host.to_ascii_lowercase()
                    && !host.contains(['/', '?', '#', ' '])
                    && !host.strip_prefix("*.").unwrap_or(host).contains('*')
            });
            if !valid {
                return Err(format!(
                    "invalid origin {origin:?}, expected \"*\" or a lowercase \
                     scheme://host[:port] such as \"https://*.example.com\""
                ));
            }
        }
        if self.methods.is_empty() {
            return Err(String::from("methods must not be empty"));
        }
        for method in &self.methods {
            if http::Method::from_bytes(method.as_bytes()).is_err() {
                return Err(format!("invalid method {method:?}"));
            }
        }
        for name in self.headers.iter().chain(&self.expose_headers) {
            if name != "*" && http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("invalid header name {name:?}"));
            }
        }
        Ok(())
    }

    /// Returns true if requests from `origin` are allowed.
    #[must_use]
    pub fn allows(&self, origin: &str) -> bool {
        self.origins.iter().any(|allowed| {
            if allowed == "*" || allowed.eq_ignore_ascii_case(origin) {
                return true;
            }
            let Some((scheme, host)) = allowed.split_once("://*.") else {
                return false;
            };
            // `https://*.example.com` matches `https://a.example.com`, not
            // `https://example.com` nor `https://evilexample.com`.
            let origin = origin.to_ascii_lowercase();
            let subdomain = origin
                .strip_prefix(&format!("{scheme}://"))
                .and_then(|rest| rest.strip_suffix(&format!(".{host}")));
            subdomain
                .is_some_and(|subdomain| !subdomain.is_empty() && !subdomain.contains(['/', ':']))
        })
    }
}

/// HTTP Basic authentication (RFC 7617) against bcrypt password hashes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BasicAuth {
    /// Realm shown in the password prompt of browsers.
    #[serde(default = "default::realm")]
    pub realm: String,

    /// User name to bcrypt hash of the password (`htpasswd -nB`).
    pub users: BTreeMap<String, String>,
}

impl BasicAuth {
    fn validate(&self) -> Result<(), String> {
        // The realm goes into a quoted string of `WWW-Authenticate`.
        if self
            .realm
            .contains(|c: char| c.is_control() || c == '"' || c == '\\')
        {
            return Err(format!(
                "realm {:?} must not contain quotes, backslashes or control characters",
                self.realm
            ));
        }
        if self.users.is_empty() {
            return Err(String::from("users must not be empty"));
        }
        for (name, hash) in &self.users {
            if name.is_empty() || name.contains(|c: char| c == ':' || c.is_control()) {
                return Err(format!(
                    "user {name:?}: a name must not be empty or contain a colon or control \
                     characters"
                ));
            }
            if hash.parse::<bcrypt::HashParts>().is_err() {
                return Err(format!(
                    "user {name:?}: the password must be a bcrypt hash (\"$2y$...\", from \
                     `htpasswd -nB`)"
                ));
            }
        }
        Ok(())
    }

    /// Hashes `password` for the `users` table, like `htpasswd -nB`.
    ///
    /// # Errors
    ///
    /// Returns a message when bcrypt cannot hash the password.
    pub fn hash_password(password: &str) -> Result<String, String> {
        bcrypt::hash(password, bcrypt::DEFAULT_COST).map_err(|err| err.to_string())
    }

    /// Returns true if `password` is the password of `user`. Slow on purpose
    /// (bcrypt): not to be called on an async runtime thread.
    #[must_use]
    pub fn verify(&self, user: &str, password: &[u8]) -> bool {
        match self.users.get(user) {
            Some(hash) => bcrypt::verify(password, hash).unwrap_or(false),
            None => {
                // The same work as for a known user, so the response time
                // does not tell which names exist.
                if let Some(hash) = self.users.values().next() {
                    let _ = bcrypt::verify(password, hash);
                }
                false
            }
        }
    }

    /// Splits the credentials of an `Authorization: Basic` header (the base64
    /// of `user:password`) into user and password.
    #[must_use]
    pub fn decode_credentials(credentials: &str) -> Option<(String, Vec<u8>)> {
        let decoded = decode_base64(credentials.trim())?;
        let colon = decoded.iter().position(|byte| *byte == b':')?;
        let user = String::from_utf8(decoded[..colon].to_vec()).ok()?;
        Some((user, decoded[colon + 1..].to_vec()))
    }
}

/// Authentication delegated to an external service (Authelia, Authentik,
/// oauth2-proxy): it gets the head of each request and decides with the
/// status of its answer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ForwardAuth {
    /// `http://` or `https://` URL of the verification endpoint.
    pub url: String,

    /// Headers of a `2xx` answer copied to the request sent to the backend,
    /// such as `Remote-User`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub copy_headers: Vec<String>,

    /// Time the service has to answer.
    #[serde(default = "default::forward_auth_timeout", with = "duration")]
    pub timeout: Duration,
}

impl ForwardAuth {
    fn validate(&self) -> Result<(), String> {
        let uri: http::Uri = self
            .url
            .parse()
            .map_err(|err| format!("invalid url {:?}: {err}", self.url))?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
            return Err(format!(
                "invalid url {:?}, expected http://host[:port][/path] or https://",
                self.url
            ));
        }
        for name in &self.copy_headers {
            if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                return Err(format!("invalid header name {name:?} in copy_headers"));
            }
        }
        if self.timeout.is_zero() {
            return Err(String::from("timeout must be greater than zero"));
        }
        Ok(())
    }
}

/// Response cache for `GET` requests.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
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
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

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

    pub fn docker_refresh() -> Duration {
        Duration::from_secs(15)
    }

    pub fn docker_listen() -> SocketAddr {
        SocketAddr::from(([0, 0, 0, 0], 80))
    }

    pub fn acme_ca() -> String {
        String::from("https://acme-v02.api.letsencrypt.org/directory")
    }

    pub fn acme_storage() -> String {
        String::from("rxh-data/certificates")
    }

    pub fn acme_renew_before() -> Duration {
        Duration::from_secs(30 * 24 * 60 * 60)
    }

    pub fn acme_dns_propagation_wait() -> Duration {
        Duration::from_secs(60)
    }

    pub fn acme_http_port() -> u16 {
        80
    }

    pub fn redirect_status() -> u16 {
        302
    }

    pub fn admin_listen() -> super::AdminListen {
        super::AdminListen::Tcp(SocketAddr::from(([127, 0, 0, 1], 2019)))
    }

    pub fn max_connections() -> usize {
        1024
    }

    pub fn log_output() -> String {
        String::from("stdout")
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

    pub fn mirror_percent() -> u8 {
        100
    }

    pub fn mirror_max_body_size() -> usize {
        1024 * 1024
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

    pub fn queue_timeout() -> Duration {
        Duration::from_secs(5)
    }

    pub fn keepalive() -> Duration {
        Duration::from_secs(120)
    }

    pub fn keepalive_idle_conns_per_host() -> usize {
        32
    }

    pub fn http2_conns_per_host() -> usize {
        1
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

    pub fn breaker_error_status() -> Vec<super::StatusMatcher> {
        vec![super::StatusMatcher::Class(5)]
    }

    pub fn breaker_latency_ratio() -> f64 {
        0.5
    }

    pub fn breaker_duration() -> Duration {
        Duration::from_secs(10)
    }

    pub fn breaker_min_requests() -> u32 {
        10
    }

    pub fn rate_limit_per() -> Duration {
        Duration::from_secs(1)
    }

    pub fn realm() -> String {
        String::from("Restricted")
    }

    pub fn forward_auth_timeout() -> Duration {
        Duration::from_secs(10)
    }

    pub fn cors_methods() -> Vec<String> {
        ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"]
            .map(String::from)
            .to_vec()
    }

    pub fn udp_idle_timeout() -> Duration {
        Duration::from_secs(60)
    }

    pub fn match_timeout() -> Duration {
        Duration::from_secs(3)
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
    fn admin_listens_on_tcp_or_a_unix_socket() {
        let config = Config::from_toml("").unwrap();
        assert_eq!(
            config.admin.listen,
            AdminListen::Tcp("127.0.0.1:2019".parse().unwrap())
        );
        assert_eq!(config.admin.token, None);

        let config = Config::from_toml("[admin]\nlisten = \"unix//run/rxh/admin.sock\"\n").unwrap();
        assert_eq!(
            config.admin.listen,
            AdminListen::Unix(PathBuf::from("/run/rxh/admin.sock"))
        );
        // The serialized form (`GET /config`, `rxh config show`) parses back.
        let written = toml::to_string(&config.admin).unwrap();
        assert_eq!(
            written,
            "enabled = true\nlisten = \"unix//run/rxh/admin.sock\"\n"
        );

        for listen in [
            "unix/relative.sock",
            "unix/",
            "unix//",
            "localhost",
            "example.com:2019",
        ] {
            let err = Config::from_toml(&format!("[admin]\nlisten = \"{listen}\"\n"))
                .expect_err(listen)
                .to_string();
            assert!(err.contains("invalid admin address"), "{err}");
        }
    }

    #[test]
    fn admin_token_is_validated_and_required_off_loopback() {
        let token = "0123456789abcdef";
        let config = Config::from_toml(&format!(
            "[admin]\nlisten = \"0.0.0.0:2019\"\ntoken = \"{token}\"\n"
        ))
        .unwrap();
        assert_eq!(config.admin.token.as_deref(), Some(token));
        // Optional on loopback and on unix sockets, accepted on both.
        Config::from_toml(&format!("[admin]\ntoken = \"{token}\"\n")).unwrap();
        Config::from_toml(&format!(
            "[admin]\nlisten = \"unix//tmp/admin.sock\"\ntoken = \"{token}\"\n"
        ))
        .unwrap();
        Config::from_toml("[admin]\nlisten = \"[::1]:2019\"\n").unwrap();
        // A disabled admin API exposes nothing.
        Config::from_toml("[admin]\nenabled = false\nlisten = \"0.0.0.0:2019\"\n").unwrap();

        let rejected = [
            ("listen = \"0.0.0.0:2019\"", "reachable from other machines"),
            (
                "listen = \"192.0.2.1:2019\"",
                "reachable from other machines",
            ),
            ("token = \"short\"", "at least 16 characters"),
            ("token = \"\"", "at least 16 characters"),
            ("token = \"0123456789 abcdef\"", "printable ASCII"),
            ("token = \"0123456789abcdef\\n\"", "printable ASCII"),
        ];
        for (line, expected) in rejected {
            let err = Config::from_toml(&format!("[admin]\n{line}\n"))
                .expect_err(line)
                .to_string();
            assert!(err.contains("admin: ") && err.contains(expected), "{err}");
        }

        let mut admin = config.admin;
        admin.redact();
        assert_eq!(admin.token.as_deref(), Some("***"));
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
    fn hash_type_and_balance_factor_keys() {
        let parse = |keys: &str| {
            Config::from_toml(&format!(
                "[upstream.api]\nbackends = [\"127.0.0.1:81\"]\npolicy = \"ip_hash\"\n{keys}\n"
            ))
        };

        let group = &parse("").unwrap().upstreams["api"];
        assert_eq!(group.hash_type, HashType::Rendezvous);
        assert_eq!(group.hash_balance_factor, None);

        let group = &parse("hash_type = \"maglev\"\nhash_balance_factor = 125")
            .unwrap()
            .upstreams["api"];
        assert_eq!(group.hash_type, HashType::Maglev);
        assert_eq!(group.hash_balance_factor, Some(125));
        assert_eq!(
            parse("hash_balance_factor = 100").unwrap().upstreams["api"].hash_balance_factor,
            Some(100)
        );

        let invalid = |keys: &str| parse(keys).unwrap_err().to_string();
        assert!(
            invalid("hash_balance_factor = 99")
                .contains("hash_balance_factor must be at least 100")
        );
        assert!(!invalid("hash_balance_factor = 1.5").is_empty());
        assert!(!invalid("hash_type = \"ketama\"").is_empty());
    }

    #[test]
    fn connection_limit_keys() {
        let parse = |keys: &str| {
            Config::from_toml(&format!(
                "[upstream.api]\nbackends = [\"127.0.0.1:81\"]\n{keys}\n"
            ))
        };
        let group = &parse("").unwrap().upstreams["api"];
        assert_eq!(
            (group.max_connections, group.queue_size, group.queue_timeout),
            (0, 0, Duration::from_secs(5))
        );
        let group = &parse("max_connections = 2\nqueue_size = 8\nqueue_timeout = \"250ms\"")
            .unwrap()
            .upstreams["api"];
        assert_eq!(
            (group.max_connections, group.queue_size, group.queue_timeout),
            (2, 8, Duration::from_millis(250))
        );

        let invalid = |keys: &str| parse(keys).unwrap_err().to_string();
        assert!(invalid("queue_size = 1").contains("queue_size needs max_connections"));
        assert!(
            invalid("max_connections = 1\nqueue_timeout = \"0s\"")
                .contains("queue_timeout must be above 0")
        );
        assert!(invalid("max_connections = -1").contains("max_connections"));
        assert!(invalid("max_connections = 1\nqueue_size = \"many\"").contains("queue_size"));
    }

    #[test]
    fn slow_start_key() {
        let parse = |value: &str| {
            Config::from_toml(&format!(
                "[upstream.api]\nbackends = [\"127.0.0.1:81\"]\nslow_start = {value}\n"
            ))
        };

        let config = parse("\"30s\"").unwrap();
        assert_eq!(
            config.upstreams["api"].slow_start,
            Some(Duration::from_secs(30))
        );
        let config = Config::from_toml("[upstream.api]\nbackends = [\"127.0.0.1:81\"]\n").unwrap();
        assert_eq!(config.upstreams["api"].slow_start, None);

        let invalid = |value: &str| parse(value).unwrap_err().to_string();
        assert!(invalid("\"0s\"").contains("slow_start must be above 0"));
        assert!(!invalid("\"soon\"").is_empty());
    }

    #[test]
    fn circuit_breaker_keys_and_defaults() {
        let parse = |breaker: &str| {
            Config::from_toml(&format!(
                "[upstream.api]\nbackends = [\"127.0.0.1:81\"]\ncircuit_breaker = {breaker}\n"
            ))
        };

        let config = parse("{ error_ratio = 0.3 }").unwrap();
        let breaker = config.upstreams["api"].circuit_breaker.as_ref().unwrap();
        assert_eq!(
            breaker,
            &CircuitBreaker {
                error_ratio: Some(0.3),
                error_status: vec![StatusMatcher::Class(5)],
                latency: None,
                latency_ratio: 0.5,
                window: Duration::from_secs(10),
                min_requests: 10,
                open_duration: Duration::from_secs(10),
            }
        );

        let config = parse(
            "{ latency = \"250ms\", latency_ratio = 1, error_status = [502, \"4xx\"], \
             window = \"1m\", min_requests = 3, open_duration = \"5s\" }",
        )
        .unwrap();
        let breaker = config.upstreams["api"].circuit_breaker.as_ref().unwrap();
        assert_eq!(breaker.error_ratio, None);
        assert_eq!(breaker.latency, Some(Duration::from_millis(250)));
        assert_eq!(breaker.latency_ratio, 1.0);
        assert_eq!(breaker.error_status.len(), 2);
        assert_eq!(breaker.window, Duration::from_secs(60));
        assert_eq!(breaker.min_requests, 3);
        assert_eq!(breaker.open_duration, Duration::from_secs(5));

        let invalid = |breaker: &str| parse(breaker).unwrap_err().to_string();
        assert!(invalid("{}").contains("needs 'error_ratio' or 'latency'"));
        assert!(invalid("{ window = \"1s\" }").contains("needs 'error_ratio' or 'latency'"));
        for ratio in ["0", "-0.1", "1.5", "nan"] {
            let message = invalid(&format!("{{ error_ratio = {ratio} }}"));
            assert!(message.contains("error_ratio must be above 0"), "{message}");
            let message = invalid(&format!("{{ latency = \"1s\", latency_ratio = {ratio} }}"));
            assert!(
                message.contains("latency_ratio must be above 0"),
                "{message}"
            );
        }
        assert!(invalid("{ latency = \"0s\" }").contains("latency must be above 0"));
        assert!(invalid("{ error_ratio = 0.5, window = \"0s\" }").contains("must be above 0"));
        assert!(
            invalid("{ error_ratio = 0.5, open_duration = \"0s\" }").contains("must be above 0")
        );
        assert!(invalid("{ error_ratio = 0.5, min_requests = 0 }").contains("at least 1"));
        assert!(invalid("{ error_ratio = 0.5, error_status = [\"9xx\"] }").contains("9xx"));
        assert!(invalid("{ error_ratio = 0.5, threshold = 3 }").contains("threshold"));
    }

    const SPLIT_POOLS: &str = "[upstream.v1]\nbackends = [\"127.0.0.1:81\"]\n\
                               [upstream.v2]\nbackends = [\"127.0.0.1:82\"]\n";

    fn split_config(server: &str) -> Result<Config, ConfigError> {
        Config::from_toml(&format!(
            "{SPLIT_POOLS}[[server]]\nlisten = \"127.0.0.1:1\"\n{server}"
        ))
    }

    fn split_error(server: &str) -> String {
        split_config(server).unwrap_err().to_string()
    }

    #[test]
    fn forward_split_between_pools() {
        let config = split_config(
            "forward = [{ pool = \"v1\", weight = 90 }, { pool = \"v2\", weight = 10 }]\n",
        )
        .unwrap();
        let Action::Forward(Forward::Split(split)) = &config.servers[0].patterns[0].action else {
            panic!("expected a split");
        };
        assert_eq!(
            split,
            &[
                WeightedPool {
                    pool: "v1".into(),
                    weight: 90
                },
                WeightedPool {
                    pool: "v2".into(),
                    weight: 10
                },
            ]
        );

        // Default weight 1, zero turns a pool off, and the split survives a
        // round trip through TOML.
        let config = split_config(
            "protocol = \"tcp\"\nforward = [{ pool = \"v1\" }, { pool = \"v2\", weight = 0 }]\n",
        )
        .unwrap();
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        let Action::Forward(Forward::Split(split)) = &again.servers[0].patterns[0].action else {
            panic!("expected a split after the round trip");
        };
        assert_eq!(split[0].weight, 1);
        assert_eq!(split[1].weight, 0);

        // A list of backends is still a list of backends.
        let config =
            split_config("forward = [{ address = \"127.0.0.1:83\", weight = 2 }]\n").unwrap();
        assert!(matches!(
            &config.servers[0].patterns[0].action,
            Action::Forward(Forward::Inline(_))
        ));
    }

    #[test]
    fn forward_split_rejects_invalid_targets() {
        let message = split_error("forward = [{ pool = \"v3\", weight = 1 }]\n");
        assert!(message.contains("unknown upstream group @v3"), "{message}");

        let message = split_error(
            "forward = [{ pool = \"v1\", weight = 0 }, { pool = \"v2\", weight = 0 }]\n",
        );
        assert!(message.contains("weight above 0"), "{message}");

        let message = split_error("forward = [{ pool = \"v1\" }, { pool = \"v1\" }]\n");
        assert!(message.contains("pool v1 is listed twice"), "{message}");

        let message = split_error("protocol = \"udp\"\nforward = [{ pool = \"v1\" }]\n");
        assert!(message.contains("udp servers cannot split"), "{message}");

        assert!(split_config("forward = [{ pool = \"v1\", weight = -1 }]\n").is_err());
        assert!(split_config("forward = [{ pool = \"v1\", share = 1 }]\n").is_err());

        // Every pool of the split is checked against the server.
        let message = Config::from_toml(&format!(
            "{SPLIT_POOLS}[upstream.sticky]\npolicy = \"cookie\"\nbackends = [\"127.0.0.1:84\"]\n\
             [[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n\
             forward = [{{ pool = \"v1\" }}, {{ pool = \"sticky\" }}]\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(message.contains("policy cookie needs HTTP"), "{message}");
    }

    #[test]
    fn mirror_parses() {
        let config = split_config(
            "forward = \"@v1\"\nmirror = { pool = \"v2\", percent = 10, max_body_size = \"64KB\" }\n",
        )
        .unwrap();
        assert_eq!(
            config.servers[0].patterns[0].mirror,
            [Mirror {
                pool: "v2".into(),
                percent: 10,
                max_body_size: 64_000,
            }]
        );

        // Defaults, a list inside a match entry, and a round trip through TOML.
        let config = split_config(
            "match = [{ forward = \"@v1\", mirror = [{ pool = \"v2\" }, { pool = \"v1\", percent = 0 }] }]\n",
        )
        .unwrap();
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        let mirror = &again.servers[0].patterns[0].mirror;
        assert_eq!(mirror.len(), 2);
        assert_eq!(
            (mirror[0].percent, mirror[0].max_body_size),
            (100, 1024 * 1024)
        );
        assert_eq!(mirror[1].percent, 0);
    }

    #[test]
    fn mirror_rejects_invalid_entries() {
        let message = split_error("forward = \"@v1\"\nmirror = { pool = \"v3\" }\n");
        assert!(
            message.contains("mirror[0]: unknown upstream group @v3"),
            "{message}"
        );

        let message = split_error("forward = \"@v1\"\nmirror = { pool = \"v2\", percent = 101 }\n");
        assert!(
            message.contains("percent must be between 0 and 100"),
            "{message}"
        );

        let message = split_error("serve = \".\"\nmirror = { pool = \"v2\" }\n");
        assert!(
            message.contains("mirror only applies to forward"),
            "{message}"
        );

        let message =
            split_error("protocol = \"tcp\"\nforward = \"@v1\"\nmirror = { pool = \"v2\" }\n");
        assert!(
            message.contains("mirror, encode and cache only apply to http"),
            "{message}"
        );

        let message = split_error("match = [{ forward = \"@v1\" }]\nmirror = { pool = \"v2\" }\n");
        assert!(message.contains("'mirror'"), "{message}");

        let message = Config::from_toml(&format!(
            "{SPLIT_POOLS}[upstream.h2]\nh2c = true\nbackends = [\"127.0.0.1:84\"]\n\
             [[server]]\nlisten = \"127.0.0.1:1\"\nproxy_protocol = \"v1\"\n\
             forward = \"@v1\"\nmirror = {{ pool = \"h2\" }}\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("mirror[0]: proxy_protocol cannot be used with h2c"),
            "{message}"
        );

        assert!(
            split_config("forward = \"@v1\"\nmirror = { pool = \"v2\", share = 1 }\n").is_err()
        );
        assert!(
            split_config("forward = \"@v1\"\nmirror = { pool = \"v2\", percent = -1 }\n").is_err()
        );
        assert!(
            split_config(
                "forward = \"@v1\"\nmirror = { pool = \"v2\", max_body_size = \"1 mile\" }\n"
            )
            .is_err()
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
    fn layer4_match_entries() {
        let tcp = |rest: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n{rest}"
            ))
        };
        let config = tcp("match = [\n\
             { sni = [\"a.test\", \"*.b.test\"], alpn = \"h2\", forward = \"x:1\" },\n\
             { protocol = \"http\", http_host = \"c.test\", forward = \"x:2\" },\n\
             { forward = \"x:3\" },\n]")
        .unwrap();
        let server = &config.servers[0];
        assert_eq!(server.match_timeout, Duration::from_secs(3));
        assert!(!server.proxy_protocol_in);
        assert_eq!(server.patterns.len(), 3);
        assert_eq!(server.patterns[0].sni, ["a.test", "*.b.test"]);
        assert_eq!(server.patterns[0].alpn, ["h2"]);
        assert_eq!(server.patterns[1].protocol, Some(L4Protocol::Http));
        assert!(!server.patterns[2].has_l4_matchers());
        let server = &tcp("proxy_protocol_in = true\nmatch_timeout = \"500ms\"\nforward = \"x:1\"")
            .unwrap()
            .servers[0];
        assert!(server.proxy_protocol_in);
        assert_eq!(server.match_timeout, Duration::from_millis(500));

        let message = |result: Result<Config, ConfigError>| match result {
            Err(err) => err.to_string(),
            Ok(_) => String::from("accepted"),
        };
        let http = |rest: &str| {
            Config::from_toml(&format!("[[server]]\nlisten = \"127.0.0.1:1\"\n{rest}"))
        };
        for (text, expected) in [("v1", ProxyProtocol::V1), ("v2", ProxyProtocol::V2)] {
            let config = http(&format!("proxy_protocol = \"{text}\"\nforward = \"x:1\"")).unwrap();
            assert_eq!(config.servers[0].proxy_protocol, Some(expected));
        }
        assert_eq!(
            http("forward = \"x:1\"").unwrap().servers[0].proxy_protocol,
            None
        );
        for (result, expected) in [
            (
                http("match = [{ sni = \"a.test\", forward = \"x:1\" }]"),
                "only apply to tcp servers",
            ),
            (
                tcp("match = [{ path = \"/a\", forward = \"x:1\" }]"),
                "not with HTTP request matchers",
            ),
            (
                tcp("match = [{ http_host = \"a.*\", forward = \"x:1\" }]"),
                "invalid host",
            ),
            (
                tcp("match = [{ protocol = \"ftp\", forward = \"x:1\" }]"),
                "unknown variant",
            ),
            (
                tcp("match = [{ sni = \"a.test\", serve = \".\" }]"),
                "can only forward",
            ),
            (
                tcp("match_timeout = \"0s\"\nforward = \"x:1\""),
                "greater than zero",
            ),
            (
                http("protocol = \"udp\"\nproxy_protocol_in = true\nforward = \"x:1\""),
                "proxy_protocol_in only applies to http and tcp",
            ),
            (
                http("protocol = \"udp\"\nproxy_protocol = \"v1\"\nforward = \"x:1\""),
                "proxy_protocol only applies to http and tcp",
            ),
            (
                http("proxy_protocol = \"v3\"\nforward = \"x:1\""),
                "unknown variant",
            ),
            (
                http("proxy_protocol = \"v2\"\nforward = { backends = [\"x:1\"], h2c = true }"),
                "proxy_protocol cannot be used with h2c",
            ),
            (
                http(
                    "proxy_protocol = \"v2\"\nforward = \"@grpc\"\n\
                     [upstream.grpc]\nbackends = [\"x:1\"]\nh2c = true",
                ),
                "proxy_protocol cannot be used with h2c",
            ),
            (
                http("protocol = \"udp\"\nmatch = [{ forward = \"x:1\" }, { forward = \"x:2\" }]"),
                "single forward",
            ),
            (
                http(
                    "forward = \"x:1\"\n[[server]]\nlisten = \"127.0.0.1:1\"\n\
                      proxy_protocol_in = true\nforward = \"x:1\"",
                ),
                "proxy_protocol_in",
            ),
        ] {
            let message = message(result);
            assert!(
                message.contains(expected),
                "{expected:?} not in {message:?}"
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
        let named_tls = format!("{tls}host = \"b.test\"\n");
        assert!(Config::from_toml(&format!("{tls}{named_tls}")).is_ok());
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
        // A later host-less certificate would never be served.
        assert!(error(format!("{named_tls}{tls}")).contains("only the first TLS server"));
    }

    #[test]
    fn duplicate_server_names_are_rejected() {
        let server = |name: &str, port: u16| {
            format!(
                "[[server]]\nname = \"{name}\"\nlisten = \"127.0.0.1:{port}\"\nforward = \"a:1\"\n"
            )
        };
        assert!(Config::from_toml(&format!("{}{}", server("a", 1), server("b", 2))).is_ok());
        let Err(ConfigError::Invalid(message)) =
            Config::from_toml(&format!("{}{}", server("web", 1), server("web", 2)))
        else {
            panic!("duplicate names accepted");
        };
        assert!(message.contains("already used by server[0]"), "{message}");
    }

    #[test]
    fn pool_checks_match_the_daemon() {
        let error = |forward: &str| match Config::from_toml(&format!(
            "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = {forward}\n"
        )) {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        };
        let message = error("{ backends = [\"a:1\"], health = { body = \"(\" } }");
        assert!(message.contains("health.body"), "{message}");
        let message = error("{ backends = [\"a:1\", \"a:1\"] }");
        assert!(message.contains("a:1 is listed twice"), "{message}");
        // Port ranges expand before the comparison.
        let message = error("{ backends = [\"a:1-3\", \"a:2\"] }");
        assert!(message.contains("a:2 is listed twice"), "{message}");
    }

    #[test]
    fn health_hooks_parse_and_validate() {
        let parse = |hooks: &str| {
            Config::from_toml(&format!(
                "[upstream.web]\nbackends = [\"a:1\"]\n\
                 [upstream.web.health.hooks]\n{hooks}\n"
            ))
        };
        let config =
            parse("command = [\"/bin/notify\", \"--pool\"]\nwebhook = \"https://hooks.test/rxh\"")
                .unwrap();
        let hooks = config.upstreams["web"]
            .health
            .as_ref()
            .and_then(|health| health.hooks.as_ref())
            .unwrap();
        assert_eq!(hooks.command, ["/bin/notify", "--pool"]);
        assert_eq!(hooks.webhook.as_deref(), Some("https://hooks.test/rxh"));
        assert!(parse("command = [\"true\"]").is_ok());
        assert!(parse("webhook = \"http://127.0.0.1:9000/\"").is_ok());

        let error = |hooks: &str| match parse(hooks) {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("{hooks}: expected a validation error, got {other:?}"),
        };
        assert!(error("").contains("needs a 'command', a 'webhook'"));
        assert!(error("command = []").contains("needs a 'command'"));
        assert!(error("command = [\"\"]").contains("program must not be empty"));
        for webhook in ["ftp://hooks.test/", "/relative", "hooks.test:80", "http://"] {
            let message = error(&format!("webhook = \"{webhook}\""));
            assert!(
                message.contains("health.hooks.webhook"),
                "{webhook}: {message}"
            );
        }
        assert!(matches!(
            parse("shell = \"echo\""),
            Err(ConfigError::Toml(_))
        ));
    }

    #[test]
    fn http_only_keys_are_rejected_elsewhere() {
        let server = |protocol: &str, rest: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"{protocol}\"\n{rest}\n"
            ))
        };
        for rest in [
            "match = [{ forward = \"a:1\", request.headers.set = { x = \"1\" } }]",
            "match = [{ forward = \"a:1\", response.headers.remove = [\"x\"] }]",
            "match = [{ forward = \"a:1\", encode = [\"gzip\"] }]",
            "match = [{ forward = \"a:1\", cache = { enabled = true } }]",
            "forward = \"a:1\"\nencode = [\"gzip\"]",
        ] {
            for protocol in ["tcp", "udp"] {
                let Err(ConfigError::Invalid(message)) = server(protocol, rest) else {
                    panic!("{protocol}: accepted {rest}");
                };
                assert!(message.contains("only apply to http servers"), "{message}");
            }
            assert!(server("http", rest).is_ok(), "{rest}");
        }
        let Err(ConfigError::Invalid(message)) =
            server("udp", "host = \"a.test\"\nforward = \"a:1\"")
        else {
            panic!("host accepted on udp");
        };
        assert!(message.contains("host does not apply"), "{message}");
        let Err(ConfigError::Invalid(message)) = server("udp", "path = \"/a\"\nforward = \"a:1\"")
        else {
            panic!("request matcher accepted on udp");
        };
        assert!(message.contains("no request matchers"), "{message}");
        assert!(server("tcp", "host = \"a.test\"\nforward = \"a:1\"").is_ok());
    }

    #[test]
    fn acme_dns_challenge_and_wildcards() {
        let config = |host: &str, acme: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
                 tls = \"auto\"\nhost = \"{host}\"\n[acme]\n{acme}\n"
            ))
        };
        let dns = |table: &str| {
            config(
                "*.a.test",
                &format!("challenge = \"dns-01\"\n[acme.dns]\n{table}"),
            )
        };
        let error = |result: Result<Config, ConfigError>| match result {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        };

        let cloudflare = dns("provider = \"cloudflare\"\napi_token = \"t0k3n\"").unwrap();
        assert_eq!(cloudflare.acme.challenge, AcmeChallenge::Dns01);
        let table = cloudflare.acme.dns.as_ref().unwrap();
        assert_eq!(table.provider, DnsProvider::Cloudflare);
        assert_eq!(table.api_token.as_deref(), Some("t0k3n"));
        assert_eq!(table.propagation_wait, Duration::from_secs(60));
        // Round trip, as `rxh config show` does.
        let text = toml::to_string(&cloudflare).unwrap();
        assert_eq!(Config::from_toml(&text).unwrap().acme, cloudflare.acme);

        let route53 = dns(
            "provider = \"route53\"\naccess_key_id = \"AKIA\"\nsecret_access_key = \"s\"\n\
             zone_id = \"Z1\"\napi_url = \"http://127.0.0.1:1\"\npropagation_wait = \"0s\"",
        )
        .unwrap();
        let table = route53.acme.dns.as_ref().unwrap();
        assert_eq!(table.provider, DnsProvider::Route53);
        assert_eq!(table.zone_id.as_deref(), Some("Z1"));
        assert_eq!(table.propagation_wait, Duration::ZERO);

        let rfc2136 = dns(
            "provider = \"rfc2136\"\nserver = \"ns1.a.test\"\ntsig_key = \"rxh.\"\n\
             tsig_secret = \"c2VjcmV0\"",
        )
        .unwrap();
        let table = rfc2136.acme.dns.as_ref().unwrap();
        assert_eq!(table.tsig_secret_bytes().unwrap().unwrap(), b"secret");
        // Unsigned updates, for servers that allow them by address.
        assert!(dns("provider = \"rfc2136\"\nserver = \"10.0.0.1:5353\"").is_ok());

        assert!(error(config("a.test", "challenge = \"dns-01\"")).contains("[acme.dns]"));
        assert!(error(dns("provider = \"cloudflare\"")).contains("needs api_token"));
        assert!(error(dns("provider = \"cloudflare\"\napi_token = \" \"")).contains("empty"));
        assert!(
            error(dns("provider = \"route53\"\naccess_key_id = \"AKIA\""))
                .contains("needs secret_access_key")
        );
        assert!(error(dns("provider = \"rfc2136\"")).contains("needs server"));
        assert!(
            error(dns(
                "provider = \"rfc2136\"\nserver = \"ns\"\napi_token = \"t\""
            ))
            .contains("api_token is not used by provider rfc2136")
        );
        assert!(
            error(dns(
                "provider = \"rfc2136\"\nserver = \"ns\"\ntsig_key = \"k\""
            ))
            .contains("go together")
        );
        assert!(
            error(dns(
                "provider = \"rfc2136\"\nserver = \"ns\"\ntsig_key = \"k\"\ntsig_secret = \"a b\""
            ))
            .contains("not base64")
        );
        assert!(
            error(dns(
                "provider = \"cloudflare\"\napi_token = \"t\"\napi_url = \"ftp://x\""
            ))
            .contains("api_url")
        );
        assert!(dns("provider = \"gandi\"").is_err());
        assert!(dns("provider = \"cloudflare\"\napi_token = \"t\"\ntypo = 1").is_err());

        // Wildcards only with DNS-01, one leading label, on a real domain.
        let token = "[acme.dns]\nprovider = \"cloudflare\"\napi_token = \"t\"";
        assert!(error(config("*.a.test", token)).contains("dns-01"));
        assert!(
            error(config(
                "*.a.test",
                &format!("challenge = \"tls-alpn-01\"\n{token}")
            ))
            .contains("dns-01")
        );
        let dns_host = |host: &str| config(host, &format!("challenge = \"dns-01\"\n{token}"));
        assert!(dns_host("a.test").is_ok());
        assert!(error(dns_host("*.test")).contains("fully qualified"));
        assert!(dns_host("*.*.a.test").is_err());

        let mut redacted = rfc2136.acme.dns.clone().unwrap();
        redacted.redact();
        assert_eq!(redacted.tsig_secret.as_deref(), Some("***"));
        assert_eq!(redacted.tsig_key.as_deref(), Some("rxh."));
        assert_eq!(redacted.api_token, None);

        // Loading the redacted copy again keeps the running secret; a new
        // secret and a removed one are taken as sent.
        let running = rfc2136.acme.dns.clone().unwrap();
        let mut restored = redacted.clone();
        restored.restore_redacted(&running);
        assert_eq!(restored, running);
        let mut changed = redacted.clone();
        changed.tsig_secret = Some(String::from("bmV3"));
        changed.restore_redacted(&running);
        assert_eq!(changed.tsig_secret.as_deref(), Some("bmV3"));
        let mut other = redacted;
        other.api_token = Some(String::from("***"));
        other.tsig_secret = None;
        other.restore_redacted(&running);
        assert_eq!(other.api_token, None, "nothing to restore from");
        assert_eq!(other.tsig_secret, None);
    }

    #[test]
    fn base64_decoding() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("+/8=").unwrap(), [0xfb, 0xff]);
        assert_eq!(decode_base64("Z"), None);
        assert_eq!(decode_base64("Zm9v-mFy"), None);
    }

    #[test]
    fn tls_options_values_and_rejections() {
        let server = |extra: &str| {
            format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{extra}\n"
            )
        };
        let parse = |extra: &str| Config::from_toml(&server(extra));
        let error = |extra: &str| match parse(extra) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("expected an error for {extra}"),
        };

        assert_eq!(parse("").unwrap().servers[0].tls_options, None);
        let defaults = TlsOptions::default();
        assert_eq!(defaults.min_version, TlsVersion::V1_2);
        assert!(defaults.ocsp_stapling);
        let config = parse("tls_options = {}").unwrap();
        assert_eq!(config.servers[0].tls_options, Some(defaults));

        let config = parse(
            "tls_options = { min_version = \"1.3\", ocsp_stapling = false, \
             cipher_suites = [\"TLS_AES_256_GCM_SHA384\"], curves = [\"x25519\", \"secp384r1\"] }",
        )
        .unwrap();
        let expected = TlsOptions {
            min_version: TlsVersion::V1_3,
            cipher_suites: vec![String::from("TLS_AES_256_GCM_SHA384")],
            curves: vec![String::from("x25519"), String::from("secp384r1")],
            ocsp_stapling: false,
        };
        assert_eq!(config.servers[0].tls_options, Some(expected.clone()));
        // Round trip through serialization, as `rxh config show` does.
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.servers[0].tls_options, Some(expected));

        for suite in TlsOptions::CIPHER_SUITES {
            parse(&format!(
                "tls_options = {{ cipher_suites = [\"{suite}\"] }}"
            ))
            .unwrap();
        }
        for curve in TlsOptions::CURVES {
            parse(&format!("tls_options = {{ curves = [\"{curve}\"] }}")).unwrap();
        }
        parse("protocol = \"tcp\"\ntls_options = { min_version = \"1.2\" }").unwrap();

        assert!(error("tls_options = { min_version = \"1.1\" }").contains("unknown variant"));
        assert!(error("tls_options = { min_version = 1.3 }").contains("tls_options"));
        assert!(error("tls_options = { nope = 1 }").contains("unknown field"));
        assert!(
            error("tls_options = { cipher_suites = [\"TLS_RSA_WITH_AES_128_CBC_SHA\"] }")
                .contains("tls_options: unknown cipher suite \"TLS_RSA_WITH_AES_128_CBC_SHA\"")
        );
        assert!(
            error("tls_options = { curves = [\"secp521r1\"] }")
                .contains("tls_options: unknown curve \"secp521r1\"")
        );
        assert!(
            error(
                "tls_options = { min_version = \"1.3\", \
                 cipher_suites = [\"TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256\"] }"
            )
            .contains("needs a TLS 1.3 cipher suite")
        );

        let plain = "[[server]]\nlisten = \"127.0.0.1:80\"\nforward = \"a:1\"\n\
                     tls_options = { min_version = \"1.3\" }\n";
        let err = Config::from_toml(plain).unwrap_err().to_string();
        assert!(err.contains("tls_options needs tls"), "{err}");

        // Servers sharing an address share the handshake, so the options too.
        let shared = |first: &str, second: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nhost = \"a.test\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{first}\n\
                 [[server]]\nlisten = \"127.0.0.1:443\"\nhost = \"b.test\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{second}\n"
            ))
        };
        let same = "tls_options = { min_version = \"1.3\" }";
        shared(same, same).unwrap();
        let err = shared(same, "").unwrap_err().to_string();
        assert!(err.contains("must have the same tls_options"), "{err}");

        // The plain HTTP redirect companion of a TLS server has no handshake.
        let redirect = parse(&format!("https_redirect = true\n{same}"))
            .unwrap()
            .with_https_redirects();
        assert_eq!(redirect.servers[1].tls_options, None);
        redirect.validate().unwrap();
    }

    #[test]
    fn client_auth_modes_and_rejections() {
        let server = |extra: &str| {
            format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{extra}\n"
            )
        };
        let parse = |extra: &str| Config::from_toml(&server(extra));
        let error = |extra: &str| match parse(extra) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("expected an error for {extra}"),
        };

        assert_eq!(parse("").unwrap().servers[0].client_auth, None);

        let config = parse("client_auth = { ca = \"ca.pem\", header = \"X-Client-Subject\" }");
        let config = config.unwrap();
        let expected = ClientAuth {
            mode: ClientAuthMode::RequireAndVerify,
            ca: vec![String::from("ca.pem")],
            header: Some(String::from("X-Client-Subject")),
        };
        assert_eq!(config.servers[0].client_auth, Some(expected.clone()));
        // Round trip through serialization, as `rxh config show` does.
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.servers[0].client_auth, Some(expected));

        for (mode, ca, expected) in [
            ("request", "", ClientAuthMode::Request),
            ("require", "", ClientAuthMode::Require),
            (
                "verify_if_given",
                ", ca = [\"a.pem\", \"b.pem\"]",
                ClientAuthMode::VerifyIfGiven,
            ),
            (
                "require_and_verify",
                ", ca = \"a.pem\"",
                ClientAuthMode::RequireAndVerify,
            ),
        ] {
            let config = parse(&format!("client_auth = {{ mode = \"{mode}\"{ca} }}")).unwrap();
            let client_auth = config.servers[0].client_auth.as_ref().unwrap();
            assert_eq!(client_auth.mode, expected);
            assert_eq!(client_auth.ca.len(), ca.matches(".pem").count());
            assert_eq!(expected.verifies(), !ca.is_empty());
        }
        assert!(ClientAuthMode::Require.is_mandatory());
        assert!(!ClientAuthMode::VerifyIfGiven.is_mandatory());

        assert!(error("client_auth = { mode = \"none\" }").contains("unknown variant"));
        assert!(error("client_auth = { ca = \"a.pem\", nope = 1 }").contains("unknown field"));
        assert!(error("client_auth = {}").contains("client_auth: this mode needs ca"));
        assert!(
            error("client_auth = { mode = \"verify_if_given\" }").contains("this mode needs ca")
        );
        assert!(
            error("client_auth = { mode = \"require\", ca = \"a.pem\" }")
                .contains("does not verify certificates")
        );
        assert!(
            error("client_auth = { mode = \"request\", header = \"X-Subject\" }")
                .contains("header needs a mode that verifies")
        );
        assert!(
            error("client_auth = { ca = \"a.pem\", header = \"bad name\" }")
                .contains("invalid header name")
        );
        assert!(
            error("protocol = \"tcp\"\nclient_auth = { ca = \"a.pem\", header = \"X-Subject\" }")
                .contains("client_auth.header only applies to http servers")
        );
        // On a tcp server the handshake policy alone is fine.
        parse("protocol = \"tcp\"\nclient_auth = { ca = \"a.pem\" }").unwrap();

        let plain = "[[server]]\nlisten = \"127.0.0.1:80\"\nforward = \"a:1\"\n\
                     client_auth = { ca = \"a.pem\" }\n";
        let err = Config::from_toml(plain).unwrap_err().to_string();
        assert!(err.contains("client_auth needs tls"), "{err}");

        // Servers sharing an address share the handshake, so the policy too.
        let shared = |first: &str, second: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nhost = \"a.test\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{first}\n\
                 [[server]]\nlisten = \"127.0.0.1:443\"\nhost = \"b.test\"\nforward = \"a:1\"\n\
                 tls = {{ cert = \"c.pem\", key = \"k.pem\" }}\n{second}\n"
            ))
        };
        let same = "client_auth = { ca = \"a.pem\" }";
        shared(same, same).unwrap();
        let err = shared(same, "").unwrap_err().to_string();
        assert!(err.contains("must have the same client_auth"), "{err}");

        // The plain HTTP redirect companion of a TLS server asks for nothing.
        let redirect = parse(&format!("https_redirect = true\n{same}"))
            .unwrap()
            .with_https_redirects();
        assert_eq!(redirect.servers.len(), 2);
        assert_eq!(redirect.servers[1].client_auth, None);
        redirect.validate().unwrap();
    }

    #[test]
    fn tls_auto_and_acme_settings() {
        let auto = |host: &str, extra: &str| {
            format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
                 tls = \"auto\"\n{host}{extra}"
            )
        };
        let error = |text: String| match Config::from_toml(&text) {
            Err(ConfigError::Invalid(message)) => message,
            other => panic!("expected a validation error, got {other:?}\n{text}"),
        };

        let config = Config::from_toml(&auto("host = [\"a.test\", \"WWW.a.test\"]\n", "")).unwrap();
        assert_eq!(config.servers[0].tls, Some(Tls::Auto));
        assert_eq!(config.acme, Acme::default());
        assert_eq!(
            config.acme.ca,
            "https://acme-v02.api.letsencrypt.org/directory"
        );
        assert_eq!(config.acme.storage, "rxh-data/certificates");
        assert_eq!(config.acme.renew_before, Duration::from_secs(30 * 86_400));
        assert_eq!(config.acme.http_port, 80);
        assert_eq!(config.acme.challenge, AcmeChallenge::Http01);

        // Round trip through serialization, as `rxh config show` does.
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("tls = \"auto\""), "{text}");
        assert_eq!(
            Config::from_toml(&text).unwrap().servers[0].tls,
            Some(Tls::Auto)
        );

        // Files and auto may share a listener, each server keeps its source.
        let files = "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
                     host = \"b.test\"\ntls = { cert = \"c.pem\", key = \"k.pem\" }\n";
        let mixed =
            Config::from_toml(&format!("{}{files}", auto("host = \"a.test\"\n", ""))).unwrap();
        assert!(matches!(&mixed.servers[1].tls, Some(Tls::Files { cert, .. }) if cert == "c.pem"));

        assert!(error(auto("", "")).contains("needs the host names"));
        assert!(error(auto("host = \"*.a.test\"\n", "")).contains("wildcard"));
        assert!(error(auto("host = \"localhost\"\n", "")).contains("fully qualified"));
        assert!(error(auto("host = \"10.0.0.1\"\n", "")).contains("IP address"));
        assert!(error(auto("host = \"a_b.test\"\n", "")).contains("not a domain name"));
        assert!(error(auto("host = \"-a.test\"\n", "")).contains("not a domain name"));
        assert!(error(auto("host = \"a..test\"\n", "")).contains("not a domain name"));
        assert!(
            error(auto("host = \"a.test\"\nprotocol = \"tcp\"\n", ""))
                .contains("only applies to http")
        );

        let with_acme = |table: &str| auto("host = \"a.test\"\n", &format!("[acme]\n{table}\n"));
        let config = Config::from_toml(&with_acme(
            "email = \"ops@a.test\"\nca = \"https://localhost:14000/dir\"\n\
             ca_root = \"pebble.pem\"\nstorage = \"/var/lib/rxh\"\n\
             renew_before = \"10d\"\nhttp_port = 5002",
        ))
        .unwrap();
        assert_eq!(config.acme.email.as_deref(), Some("ops@a.test"));
        assert_eq!(config.acme.ca_root.as_deref(), Some("pebble.pem"));
        assert_eq!(config.acme.renew_before, Duration::from_secs(10 * 86_400));
        assert_eq!(config.acme.http_port, 5002);

        assert!(error(with_acme("ca = \"http://ca.test/dir\"")).contains("https://"));
        assert!(error(with_acme("email = \"nobody\"")).contains("e-mail"));
        assert!(error(with_acme("renew_before = \"0s\"")).contains("renew_before"));
        assert!(error(with_acme("http_port = 0")).contains("http_port"));
        assert!(error(with_acme("storage = \" \"")).contains("storage"));
        assert!(Config::from_toml(&with_acme("typo = 1")).is_err());

        let challenge = |value: &str| {
            Config::from_toml(&with_acme(&format!("challenge = {value}")))
                .map(|config| config.acme.challenge)
        };
        assert_eq!(challenge("\"http-01\"").unwrap(), AcmeChallenge::Http01);
        assert_eq!(
            challenge("\"tls-alpn-01\"").unwrap(),
            AcmeChallenge::TlsAlpn01
        );
        for rejected in ["\"dns-02\"", "\"TLS-ALPN-01\"", "\"\"", "1"] {
            assert!(challenge(rejected).is_err(), "{rejected}");
        }
        let mut config = Config::from_toml(&with_acme("challenge = \"tls-alpn-01\"")).unwrap();
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("challenge = \"tls-alpn-01\""), "{text}");
        config = Config::from_toml(&text).unwrap();
        assert_eq!(config.acme.challenge, AcmeChallenge::TlsAlpn01);

        // Only "auto" or a table, and the table still rejects unknown keys.
        let tls = |value: &str| {
            Config::from_toml(&format!(
                "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\ntls = {value}\n"
            ))
            .unwrap_err()
            .to_string()
        };
        assert!(tls("\"manual\"").contains("\"auto\" or a table"));
        assert!(tls("{ cert = \"c\", key = \"k\", extra = 1 }").contains("unknown field"));
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
    fn timeouts_parse_and_reject_zero() {
        let config = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nidle_timeout = \"5s\"\n\
             forward = { backends = [\"a:1\"], read_timeout = \"250ms\" }",
        )
        .expect("valid");
        assert_eq!(config.servers[0].idle_timeout, Some(Duration::from_secs(5)));
        assert_eq!(config.servers[0].stream_timeout, None);

        let err = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = { backends = [\"a:1\"], read_timeout = \"0s\" }",
        )
        .expect_err("zero is invalid");
        assert!(err.to_string().contains("read_timeout"), "{err}");
    }

    #[test]
    fn fallback_accepts_text_and_nested_tables() {
        let config = Config::from_toml(
            r#"
            [upstream.text]
            fallback = "round_robin"
            backends = ["127.0.0.1:1"]

            [upstream.nested]
            policy = "header"
            key = "x-a"
            fallback = { policy = "query", key = "q", fallback = { policy = "cookie", key = "sid", secret = "s", fallback = "first" } }
            retry_match = { method = ["POST"], status = ["502", "5xx"] }
            backends = ["127.0.0.1:1"]
            "#,
        )
        .expect("valid");
        assert_eq!(
            config.upstreams["text"].fallback.as_ref().unwrap().policy,
            Policy::RoundRobin
        );
        let nested = &config.upstreams["nested"];
        let query = nested.fallback.as_ref().unwrap();
        assert_eq!(query.key.as_deref(), Some("q"));
        let cookie = query.fallback.as_ref().unwrap();
        assert_eq!(cookie.policy, Policy::Cookie);
        assert_eq!(cookie.fallback.as_ref().unwrap().policy, Policy::First);
        assert_eq!(nested.cookie_secret(), Some("s"));
        let retry = nested.retry_match.as_ref().unwrap();
        assert!(retry.retries_method(&http::Method::POST));
        assert!(!retry.retries_method(&http::Method::GET));
        assert!(
            retry.retries_status(502) && retry.retries_status(500) && !retry.retries_status(404)
        );
    }

    #[test]
    fn nested_fallback_and_header_rules_are_validated() {
        for extra in [
            "fallback = { policy = \"query\" }",
            "fallback = { policy = \"first\", fallback = { policy = \"header\" } }",
            "fallback = { policy = \"first\", bogus = 1 }",
            "retry_match = { method = [\"BAD METHOD\"] }",
            "retry_match = { status = [\"nope\"] }",
        ] {
            let text = format!("[upstream.a]\nbackends = [\"127.0.0.1:1\"]\n{extra}\n");
            assert!(Config::from_toml(&text).is_err(), "{extra}");
        }
        let text = "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"127.0.0.1:1\"\n";
        for extra in [
            "request.headers.remove = [\"*\"]",
            "response.headers.remove = [\"*\"]",
            "request.headers.add = { \"bad name\" = \"x\" }",
            "request.headers.replace = { \"x\" = { search = \"(\", replace = \"\" } }",
            "response.headers.replace = { \"x\" = { search = \"(\", replace = \"\" } }",
        ] {
            assert!(
                Config::from_toml(&format!("{text}{extra}\n")).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn included_files_merge_pools_and_servers() {
        let mut config =
            Config::parse("include = [\"a.toml\"]\n[upstream.web]\nbackends = [\"127.0.0.1:1\"]\n")
                .expect("main parses");
        let part = IncludedConfig::from_toml(
            "[upstream.api]\nbackends = [\"127.0.0.1:2\"]\n\
             [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"@api\"\n",
        )
        .expect("part parses");
        config.merge(part, "a.toml").expect("merges");
        config
            .validate()
            .expect("pool references resolve after the merge");
        assert_eq!(config.include, ["a.toml"]);
        assert_eq!(config.upstreams.len(), 2);

        let duplicate = IncludedConfig::from_toml("[upstream.web]\nbackends = [\"127.0.0.1:3\"]\n")
            .expect("parses");
        let err = config
            .merge(duplicate, "b.toml")
            .expect_err("duplicate pool");
        assert!(err.to_string().contains("pool web"), "{err}");

        assert!(IncludedConfig::from_toml("[admin]\nenabled = false\n").is_err());
        assert!(IncludedConfig::from_toml("include = [\"x\"]\n").is_err());
    }

    #[test]
    fn docker_section_has_defaults_and_validates_the_endpoint() {
        assert_eq!(Config::from_toml("").expect("valid").docker, None);

        let config = Config::from_toml("[docker]\n").expect("valid");
        let docker = config.docker.as_ref().expect("enabled by the table");
        assert_eq!(docker.provider, ContainerProvider::Docker);
        assert_eq!(docker.endpoint(), "unix:///var/run/docker.sock");
        assert_eq!(docker.listen, "0.0.0.0:80".parse().expect("address"));
        assert_eq!(docker.refresh, Duration::from_secs(15));
        let text = toml::to_string(&config).expect("serializes");
        assert_eq!(
            Config::from_toml(&text).expect("round trip").docker,
            config.docker
        );

        for endpoint in [
            "unix:///run/user/1000/docker.sock",
            "tcp://127.0.0.1:2375",
            "tcp://docker:2375",
        ] {
            let text =
                format!("[docker]\nendpoint = \"{endpoint}\"\nlisten = \"127.0.0.1:8080\"\n");
            let docker = Config::from_toml(&text)
                .expect(endpoint)
                .docker
                .expect("set");
            assert_eq!(docker.endpoint(), endpoint);
            assert_eq!(docker.listen.port(), 8080);
        }

        for endpoint in [
            "",
            "/var/run/docker.sock",
            "unix://relative.sock",
            "tcp://docker",
            "http://127.0.0.1:2375",
            "tcp://unix//x",
        ] {
            let err = Config::from_toml(&format!("[docker]\nendpoint = \"{endpoint}\"\n"))
                .expect_err(endpoint)
                .to_string();
            assert!(err.contains("docker: endpoint must be"), "{err}");
        }
        assert!(Config::from_toml("[docker]\nlisten = \"80\"\n").is_err());
        assert!(Config::from_toml("[docker]\nnetwork = \"web\"\n").is_err());
        assert!(IncludedConfig::from_toml("[docker]\n").is_err());
    }

    #[test]
    fn catalog_sections_have_defaults_and_validate() {
        let config = Config::from_toml("").expect("valid");
        assert_eq!(
            (config.consul, config.etcd, config.nomad),
            (None, None, None)
        );

        let config = Config::from_toml(
            "[consul]
[etcd]
[nomad]
",
        )
        .expect("valid");
        for (kind, endpoint) in [
            (CatalogKind::Consul, "http://127.0.0.1:8500"),
            (CatalogKind::Etcd, "http://127.0.0.1:2379"),
            (CatalogKind::Nomad, "http://127.0.0.1:4646"),
        ] {
            let catalog = config.catalog(kind).expect("enabled by the table");
            assert_eq!(catalog.endpoint(kind), endpoint);
            assert_eq!(catalog.listen, "0.0.0.0:80".parse().expect("address"));
            assert_eq!(catalog.refresh, Duration::from_secs(15));
        }
        assert_eq!(config.etcd.as_ref().expect("set").prefix(), "rxh/");

        let text = "[consul]
endpoint = \"http://consul.service:8500\"\ntoken = \"secret\"\n\
                    listen = \"127.0.0.1:8080\"\nrefresh = \"2s\"\n\
                    [etcd]\nendpoint = \"http://[::1]:2379\"\nprefix = \"apps/\"\n\
                    [nomad]\ntoken = \"t\"\n";
        let config = Config::from_toml(text).expect("valid");
        let consul = config.consul.as_ref().expect("set");
        assert_eq!(
            consul.endpoint(CatalogKind::Consul),
            "http://consul.service:8500"
        );
        assert_eq!(consul.token.as_deref(), Some("secret"));
        assert_eq!(consul.listen.port(), 8080);
        assert_eq!(consul.refresh, Duration::from_secs(2));
        assert_eq!(config.etcd.as_ref().expect("set").prefix(), "apps/");
        let round_trip =
            Config::from_toml(&toml::to_string(&config).expect("serializes")).expect("round trip");
        assert_eq!(
            (round_trip.consul, round_trip.etcd, round_trip.nomad),
            (config.consul, config.etcd, config.nomad)
        );

        for (text, reason) in [
            (
                "[consul]\nendpoint = \"127.0.0.1:8500\"\n",
                "consul: endpoint must be",
            ),
            (
                "[consul]\nendpoint = \"https://consul:8501\"\n",
                "consul: endpoint must be",
            ),
            (
                "[nomad]\nendpoint = \"http://nomad:4646/v1\"\n",
                "nomad: endpoint must be",
            ),
            (
                "[etcd]\nendpoint = \"unix:///run/etcd.sock\"\n",
                "etcd: endpoint must be",
            ),
            (
                "[nomad]\nrefresh = \"0s\"\n",
                "nomad: refresh must be longer than 0s",
            ),
            ("[etcd]\ntoken = \"t\"\n", "etcd: token is not supported"),
            ("[etcd]\nprefix = \"\"\n", "etcd: prefix cannot be empty"),
            (
                "[consul]\nprefix = \"rxh/\"\n",
                "consul: prefix is only used by [etcd]",
            ),
            ("[nomad]\nnamespace = \"x\"\n", "unknown field"),
            ("[consul]\nlisten = \"80\"\n", "listen"),
        ] {
            let err = Config::from_toml(text).expect_err(text).to_string();
            assert!(err.contains(reason), "{text}: {err}");
        }
        for section in ["[consul]\n", "[etcd]\n", "[nomad]\n", "[kubernetes]\n"] {
            assert!(IncludedConfig::from_toml(section).is_err(), "{section}");
        }
    }

    #[test]
    fn kubernetes_section_has_defaults_and_validates() {
        assert_eq!(Config::from_toml("").expect("valid").kubernetes, None);
        let config = Config::from_toml("[kubernetes]\n").expect("valid");
        let kubernetes = config.catalog(CatalogKind::Kubernetes).expect("set");
        // The address `kubectl proxy` listens at.
        assert_eq!(
            kubernetes.endpoint(CatalogKind::Kubernetes),
            "http://127.0.0.1:8001"
        );
        assert_eq!(kubernetes.listen, "0.0.0.0:80".parse().expect("address"));
        assert_eq!(kubernetes.refresh, Duration::from_secs(15));
        assert_eq!(kubernetes.ingress_class, None);

        let text = "[kubernetes]\nendpoint = \"http://10.0.0.1:8080\"\ntoken = \"t\"\n\
                    ingress_class = \"rxh\"\nlisten = \"127.0.0.1:8080\"\nrefresh = \"5s\"\n";
        let config = Config::from_toml(text).expect("valid");
        let kubernetes = config.kubernetes.as_ref().expect("set");
        assert_eq!(kubernetes.token.as_deref(), Some("t"));
        assert_eq!(kubernetes.ingress_class.as_deref(), Some("rxh"));
        assert_eq!(kubernetes.refresh, Duration::from_secs(5));
        let round_trip =
            Config::from_toml(&toml::to_string(&config).expect("serializes")).expect("round trip");
        assert_eq!(round_trip.kubernetes, config.kubernetes);

        for (text, reason) in [
            (
                "[kubernetes]\nendpoint = \"https://10.96.0.1:443\"\n",
                "kubernetes: endpoint must be",
            ),
            (
                "[kubernetes]\nrefresh = \"0s\"\n",
                "kubernetes: refresh must be longer than 0s",
            ),
            (
                "[kubernetes]\ningress_class = \"\"\n",
                "kubernetes: ingress_class cannot be empty",
            ),
            (
                "[kubernetes]\nprefix = \"rxh/\"\n",
                "kubernetes: prefix is only used by [etcd]",
            ),
            (
                "[consul]\ningress_class = \"rxh\"\n",
                "consul: ingress_class is only used by [kubernetes]",
            ),
            ("[kubernetes]\nnamespace = \"x\"\n", "unknown field"),
        ] {
            let err = Config::from_toml(text).expect_err(text).to_string();
            assert!(err.contains(reason), "{text}: {err}");
        }
    }

    #[test]
    fn docker_section_selects_podman_or_swarm() {
        let docker = |extra: &str| -> Docker {
            Config::from_toml(&format!("[docker]\n{extra}"))
                .expect(extra)
                .docker
                .expect("set")
        };
        let podman = docker("provider = \"podman\"\n");
        assert_eq!(podman.provider, ContainerProvider::Podman);
        assert_eq!(podman.endpoint(), "unix:///run/podman/podman.sock");
        let rootless = docker(
            "provider = \"podman\"\nendpoint = \"unix:///run/user/1000/podman/podman.sock\"\n",
        );
        assert_eq!(
            rootless.endpoint(),
            "unix:///run/user/1000/podman/podman.sock"
        );

        let swarm = docker("provider = \"swarm\"\nrefresh = \"5s\"\n");
        assert_eq!(swarm.provider, ContainerProvider::Swarm);
        assert_eq!(swarm.endpoint(), "unix:///var/run/docker.sock");
        assert_eq!(swarm.refresh, Duration::from_secs(5));
        let text = toml::to_string(&Config {
            docker: Some(swarm.clone()),
            ..Config::default()
        })
        .expect("serializes");
        assert_eq!(
            Config::from_toml(&text).expect("round trip").docker,
            Some(swarm)
        );

        for (extra, reason) in [
            ("provider = \"kubernetes\"\n", "unknown variant"),
            ("provider = \"Docker\"\n", "unknown variant"),
            (
                "refresh = \"0s\"\n",
                "docker: refresh must be longer than 0s",
            ),
            ("refresh = \"soon\"\n", "refresh"),
            (
                "provider = \"podman\"\nendpoint = \"podman.sock\"\n",
                "docker: endpoint must be",
            ),
        ] {
            let err = Config::from_toml(&format!("[docker]\n{extra}"))
                .expect_err(extra)
                .to_string();
            assert!(err.contains(reason), "{extra}: {err}");
        }
    }

    #[test]
    fn watch_is_a_boolean_off_by_default() {
        let default = Config::from_toml("").expect("valid");
        assert!(!default.watch);
        let text = toml::to_string(&default).expect("serializes");
        assert!(!text.contains("watch"), "{text}");

        let watched = Config::from_toml("watch = true\n").expect("valid");
        assert!(watched.watch);
        let text = toml::to_string(&watched).expect("serializes");
        assert!(text.contains("watch = true"), "{text}");
        assert!(!Config::from_toml("watch = false\n").expect("valid").watch);

        for rejected in ["\"true\"", "\"yes\"", "1", "\"1s\""] {
            assert!(
                Config::from_toml(&format!("watch = {rejected}\n")).is_err(),
                "{rejected}"
            );
        }
        // Set in the main file only.
        assert!(IncludedConfig::from_toml("watch = true\n").is_err());
    }

    #[test]
    fn runtime_is_shared_or_per_core() {
        let default = Config::from_toml("").expect("valid");
        assert_eq!(default.runtime, Runtime::Shared);
        let text = toml::to_string(&default).expect("serializes");
        assert!(!text.contains("runtime"), "{text}");

        let shared = Config::from_toml("runtime = \"shared\"\n").expect("valid");
        assert_eq!(shared.runtime, Runtime::Shared);

        let per_core = Config::from_toml("runtime = \"per_core\"\n").expect("valid");
        assert_eq!(per_core.runtime, Runtime::PerCore);
        let text = toml::to_string(&per_core).expect("serializes");
        assert!(text.contains("runtime = \"per_core\""), "{text}");

        for rejected in ["\"per-core\"", "\"percore\"", "\"\"", "4", "true"] {
            assert!(
                Config::from_toml(&format!("runtime = {rejected}\n")).is_err(),
                "{rejected}"
            );
        }
        // A process wide choice: not for included files.
        assert!(IncludedConfig::from_toml("runtime = \"per_core\"\n").is_err());
    }

    #[test]
    fn serve_string_and_table() {
        let config = Config::from_toml(
            r#"
[[server]]
listen = "127.0.0.1:0"
encode = ["zstd", "gzip"]
serve = { root = ".", browse = false, hide = [".git"], precompressed = ["br"] }
"#,
        )
        .expect("valid");
        let pattern = &config.servers[0].patterns[0];
        assert_eq!(pattern.encode, [Encoding::Zstd, Encoding::Gzip]);
        let Action::Serve(files) = &pattern.action else {
            panic!("serve expected");
        };
        assert!(!files.browse);
        assert_eq!(files.index, ["index.html"]);
        assert_eq!(files.hide, [".git"]);
        assert_eq!(files.precompressed, [Encoding::Br]);

        let config = Config::from_toml("[[server]]\nlisten = \"127.0.0.1:0\"\nserve = \"/srv\"\n")
            .expect("valid");
        let Action::Serve(files) = &config.servers[0].patterns[0].action else {
            panic!("serve expected");
        };
        assert!(files.browse && files.root == "/srv");

        let slash =
            "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = { root = \".\", hide = [\"a/b\"] }\n";
        assert!(Config::from_toml(slash).is_err());

        let bad = "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = { root = \".\", nope = 1 }\n";
        assert!(Config::from_toml(bad).is_err());
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

    #[test]
    fn log_table_parses_and_rejects_unknown_keys() {
        let config =
            Config::from_toml("[log]\nformat = \"json\"\noutput = \"/tmp/a.log\"").expect("valid");
        assert_eq!(config.log.format, LogFormat::Json);
        assert!(config.log.access);
        assert_eq!(Config::default().log.output, "stdout");
        assert!(Config::from_toml("[log]\nfile = \"x\"").is_err());
        assert!(Config::from_toml("[log]\noutput = \"\"").is_err());
    }

    fn invalid(text: &str) -> String {
        Config::from_toml(text)
            .expect_err("configuration must be rejected")
            .to_string()
    }

    #[test]
    fn redirect_rewrite_and_method_parse() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            redirect = "https://example.com{uri}"

            [[server]]
            listen = "127.0.0.1:8001"
            match = [
                { uri = "/old", redirect = { to = "/new{path}?{query}", status = 301 } },
                { uri = "/api", rewrite = "/v2{path}", request = { method = "post" }, forward = "127.0.0.1:1" },
            ]
            "#,
        )
        .unwrap();

        let Action::Redirect(redirect) = &config.servers[0].patterns[0].action else {
            panic!("expected a redirect");
        };
        assert_eq!(redirect.to, "https://example.com{uri}");
        assert_eq!(redirect.status, 302, "temporary by default, like Caddy");

        let patterns = &config.servers[1].patterns;
        let Action::Redirect(redirect) = &patterns[0].action else {
            panic!("expected a redirect");
        };
        assert_eq!(
            (redirect.to.as_str(), redirect.status),
            ("/new{path}?{query}", 301)
        );
        assert_eq!(patterns[1].rewrite.as_deref(), Some("/v2{path}"));
        assert_eq!(patterns[1].request.method.as_deref(), Some("post"));
        assert_eq!(patterns[0].rewrite, None);

        // The single pattern form takes `rewrite` too.
        let config = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:1\"\nrewrite = \"/x\"\nforward = \"127.0.0.1:2\"",
        )
        .unwrap();
        assert_eq!(config.servers[0].patterns[0].rewrite.as_deref(), Some("/x"));
    }

    #[test]
    fn invalid_redirect_rewrite_and_method_are_rejected() {
        let server = |options: &str| format!("[[server]]\nlisten = \"127.0.0.1:1\"\n{options}");
        let forward = |options: &str| server(&format!("forward = \"127.0.0.1:2\"\n{options}"));

        for (text, expected) in [
            (
                server("redirect = { to = \"/x\", status = 200 }"),
                "status 200",
            ),
            (server("redirect = { to = \"/x\", code = 301 }"), "redirect"),
            (server("redirect = \"/{nope}\""), "unknown placeholder"),
            (server("redirect = \"\""), "must not be empty"),
            (server("redirect = \"/x\"\nserve = \".\""), "either"),
            (
                server("redirect = \"/x\"\nrewrite = \"/y\""),
                "do not apply to redirect",
            ),
            (
                server("redirect = \"/x\"\nrequest = { method = \"GET\" }"),
                "do not apply",
            ),
            (
                server("protocol = \"tcp\"\nredirect = \"/x\""),
                "can only forward",
            ),
            (forward("rewrite = \"x\""), "starting with /"),
            (forward("rewrite = \"/{host\""), "unclosed"),
            (forward("rewrite = \"/a#b\""), "starting with /"),
            (
                forward("request = { method = \"GE T\" }"),
                "not an HTTP method",
            ),
            (forward("request = { method = \"\" }"), "not an HTTP method"),
            (
                forward("protocol = \"tcp\"\nrewrite = \"/x\""),
                "only apply to http",
            ),
            (
                server("match = [{ forward = \"127.0.0.1:2\" }]\nrewrite = \"/x\""),
                "inside each 'match' entry",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}\n=> {message}");
        }
    }

    #[test]
    fn handle_response_parses() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            forward = "127.0.0.1:1"
            handle_response = [
                { status = 404, replace_status = 200 },
                { status = ["5xx", 404], header = { x-accel-redirect = "*" }, serve = "/srv", rewrite = "{header.x-accel-redirect}" },
                { status = 401, redirect = { to = "/login?from={path}&status={status}", status = 303 } },
                { copy_response = true },
                { status = "5xx", copy_response = 500 },
            ]
            "#,
        )
        .unwrap();
        let handlers = &config.servers[0].patterns[0].handle_response;
        assert_eq!(handlers.len(), 5);
        assert_eq!(handlers[0].status, [StatusMatcher::Code(404)]);
        assert_eq!(handlers[0].new_status(), Some(200));
        assert_eq!(
            handlers[1].status,
            [StatusMatcher::Class(5), StatusMatcher::Code(404)]
        );
        assert_eq!(handlers[1].serve.as_ref().unwrap().root, "/srv");
        assert_eq!(
            handlers[1].rewrite.as_deref(),
            Some("{header.x-accel-redirect}")
        );
        assert_eq!(handlers[2].redirect.as_ref().unwrap().status, 303);
        assert_eq!(handlers[3].copy_response, Some(CopyResponse::Enabled(true)));
        assert_eq!(handlers[3].new_status(), None);
        assert_eq!(handlers[4].new_status(), Some(500));

        // The serialized form (admin API, `rxh service`) parses back.
        let text = toml::to_string(&config).unwrap();
        let again = Config::from_toml(&text).unwrap();
        assert_eq!(again.servers[0].patterns[0].handle_response.len(), 5);
    }

    #[test]
    fn handle_response_matches_status_and_headers() {
        let handler = |text: &str| -> HandleResponse { toml::from_str(text).unwrap() };
        let mut headers = http::HeaderMap::new();
        headers.insert("x-kind", "error".parse().unwrap());

        let any = handler("copy_response = true");
        assert!(any.matches(200, &headers) && any.matches(503, &http::HeaderMap::new()));

        let by_status = handler("status = [404, \"5xx\"]\nreplace_status = 200");
        assert!(by_status.matches(404, &headers) && by_status.matches(502, &headers));
        assert!(!by_status.matches(403, &headers));

        // Status and headers must both match; header names ignore case.
        let both = handler("status = 500\nheader = { X-Kind = \"error\" }\nreplace_status = 200");
        assert!(both.matches(500, &headers));
        assert!(!both.matches(404, &headers));
        assert!(!both.matches(500, &http::HeaderMap::new()));

        let other_value = handler("header = { x-kind = \"other\" }\nreplace_status = 200");
        assert!(!other_value.matches(500, &headers));
        let present = handler("header = { x-kind = \"*\" }\nreplace_status = 200");
        assert!(present.matches(200, &headers) && !present.matches(200, &http::HeaderMap::new()));
    }

    #[test]
    fn invalid_handle_response_is_rejected() {
        let server = |options: &str| format!("[[server]]\nlisten = \"127.0.0.1:1\"\n{options}");
        let forward = |handler: &str| {
            server(&format!(
                "forward = \"127.0.0.1:2\"\nhandle_response = [{{ {handler} }}]"
            ))
        };

        for (text, expected) in [
            (forward("status = 404"), "exactly one of"),
            (
                forward("replace_status = 200, copy_response = true"),
                "exactly one of",
            ),
            (
                forward("serve = \".\", redirect = \"/x\""),
                "exactly one of",
            ),
            (forward("replace_status = 99"), "between 200 and 599"),
            (forward("replace_status = 600"), "between 200 and 599"),
            (forward("copy_response = 1000"), "between 200 and 599"),
            (forward("copy_response = false"), "true or a status"),
            (forward("status = 700, replace_status = 200"), "status"),
            (forward("status = \"9xx\", replace_status = 200"), "status"),
            (
                forward("header = { \"bad name\" = \"x\" }, replace_status = 200"),
                "invalid header name",
            ),
            (
                forward("replace_status = 200, rewrite = \"/x\""),
                "rewrite needs serve",
            ),
            (
                forward("serve = \".\", rewrite = \"/{nope}\""),
                "unknown placeholder",
            ),
            (
                forward("serve = { root = \".\", hide = [\"a/b\"] }"),
                "serve.hide",
            ),
            (
                forward("redirect = { to = \"/x\", status = 200 }"),
                "status 200",
            ),
            (forward("redirect = \"/{header.bad name}\""), "redirect"),
            (forward("replace_status = 200, respond = \"x\""), "unknown"),
            (
                server("serve = \".\"\nhandle_response = [{ replace_status = 200 }]"),
                "only applies to forward",
            ),
            (
                server(
                    "protocol = \"tcp\"\nforward = \"127.0.0.1:2\"\n\
                     handle_response = [{ replace_status = 200 }]",
                ),
                "only apply to http",
            ),
            (
                server(
                    "match = [{ forward = \"127.0.0.1:2\" }]\n\
                     handle_response = [{ replace_status = 200 }]",
                ),
                "inside each 'match' entry",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}\n=> {message}");
        }
    }

    const TLS_FILES: &str = "tls = { cert = \"c.pem\", key = \"k.pem\" }";

    #[test]
    fn https_redirect_adds_a_plain_server_on_the_http_port() {
        let config = Config::from_toml(&format!(
            "[[server]]\nlisten = [\"10.0.0.1:443\", \"10.0.0.2:443\"]\nhost = \"a.test\"\n\
             tls = \"auto\"\nforward = \"127.0.0.1:1\"\n\
             [[server]]\nlisten = \"10.0.0.1:8443\"\nhost = \"b.test\"\n{TLS_FILES}\n\
             https_redirect = true\nforward = \"127.0.0.1:1\"\n\
             [[server]]\nlisten = \"10.0.0.1:9443\"\n{TLS_FILES}\nforward = \"127.0.0.1:1\"\n"
        ))
        .unwrap();
        assert_eq!(
            config.servers.len(),
            3,
            "the parsed configuration is as written"
        );

        let effective = config.with_https_redirects();
        // Automatic for `tls = "auto"`, explicit for b.test, none for the last.
        assert_eq!(effective.servers.len(), 5);
        let target = |server: &Server| match &server.patterns[0].action {
            Action::Redirect(redirect) => (redirect.to.clone(), redirect.status),
            other => panic!("expected a redirect, got {other:?}"),
        };

        let auto = &effective.servers[3];
        let listen: Vec<String> = auto.listen.iter().map(SocketAddr::to_string).collect();
        assert_eq!(listen, ["10.0.0.1:80", "10.0.0.2:80"]);
        assert_eq!(auto.host, ["a.test"]);
        assert_eq!(auto.tls, None);
        assert_eq!(target(auto), (String::from("https://{host}{uri}"), 308));

        let explicit = &effective.servers[4];
        assert_eq!(explicit.listen[0].to_string(), "10.0.0.1:80");
        assert_eq!(explicit.host, ["b.test"]);
        assert_eq!(
            target(explicit),
            (String::from("https://{host}:8443{uri}"), 308)
        );
        effective
            .validate()
            .expect("the effective configuration is valid");
    }

    #[test]
    fn https_redirect_can_be_disabled_and_follows_http_port() {
        let auto = |options: &str| {
            Config::from_toml(&format!(
                "[acme]\nhttp_port = 8080\n[[server]]\nlisten = \"10.0.0.1:443\"\n\
                 host = \"a.test\"\ntls = \"auto\"\nforward = \"127.0.0.1:1\"\n{options}"
            ))
            .unwrap()
            .with_https_redirects()
        };
        let enabled = auto("");
        assert_eq!(enabled.servers[1].listen[0].to_string(), "10.0.0.1:8080");
        assert_eq!(auto("https_redirect = false").servers.len(), 1);
        // The automatic redirect steps aside when a tcp server owns the port.
        let blocked = auto(
            "[[server]]\nlisten = \"0.0.0.0:8080\"\nprotocol = \"tcp\"\nforward = \"127.0.0.1:1\"",
        );
        assert_eq!(blocked.servers.len(), 2);
    }

    #[test]
    fn invalid_https_redirect_is_rejected() {
        let server = |listen: &str, options: &str| {
            format!(
                "[[server]]\nlisten = \"{listen}\"\nforward = \"127.0.0.1:1\"\n\
                 https_redirect = true\n{options}\n"
            )
        };
        for (text, expected) in [
            (server("127.0.0.1:443", ""), "http servers with tls"),
            (
                server("127.0.0.1:443", &format!("protocol = \"tcp\"\n{TLS_FILES}")),
                "http servers with tls",
            ),
            (server("127.0.0.1:0", TLS_FILES), "fixed port"),
            (
                // The explicit option is an error where the automatic one steps aside.
                format!(
                    "{}[[server]]\nlisten = \"127.0.0.1:80\"\nprotocol = \"tcp\"\n\
                     forward = \"127.0.0.1:1\"\n",
                    server("127.0.0.1:443", TLS_FILES)
                ),
                "port 80",
            ),
            (
                server("127.0.0.1:443", "https_redirect = \"yes\""),
                "https_redirect",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}\n=> {message}");
        }
    }

    #[test]
    fn connection_limit_buffers_and_body_size_parse() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            request = { max_body_size = "10MB" }
            forward = { backends = ["127.0.0.1:1"], max_conns_per_host = 4, request_buffers = "64KiB", response_buffers = 4096 }

            [[server]]
            listen = "127.0.0.1:8001"
            match = [{ uri = "/up", request = { max_body_size = 1024 }, forward = "127.0.0.1:1" }]
            "#,
        )
        .unwrap();

        let pattern = &config.servers[0].patterns[0];
        assert_eq!(pattern.request.max_body_size, Some(10_000_000));
        let Action::Forward(Forward::Inline(group)) = &pattern.action else {
            panic!("expected an inline pool");
        };
        assert_eq!(group.max_conns_per_host, 4);
        assert_eq!(group.request_buffers, 65_536);
        assert_eq!(group.response_buffers, 4096);
        assert_eq!(
            config.servers[1].patterns[0].request.max_body_size,
            Some(1024)
        );

        // Off by default.
        let group = UpstreamGroup::default();
        assert_eq!(
            (
                group.max_conns_per_host,
                group.request_buffers,
                group.response_buffers
            ),
            (0, 0, 0)
        );
        assert_eq!(RequestConfig::default().max_body_size, None);
    }

    #[test]
    fn flush_interval_and_stream_close_delay_parse() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            stream_close_delay = "5s"
            forward = { backends = ["127.0.0.1:1"], flush_interval = "100ms" }
            "#,
        )
        .unwrap();

        let server = &config.servers[0];
        assert_eq!(server.stream_close_delay, Some(Duration::from_secs(5)));
        let Action::Forward(Forward::Inline(group)) = &server.patterns[0].action else {
            panic!("expected an inline pool");
        };
        assert_eq!(group.flush_interval, Some(Duration::from_millis(100)));

        // Zero closes tunnels right away, like Caddy's default.
        let config = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nstream_close_delay = \"0s\"\nforward = \"a:1\"",
        )
        .unwrap();
        assert_eq!(config.servers[0].stream_close_delay, Some(Duration::ZERO));

        // Off by default.
        assert_eq!(UpstreamGroup::default().flush_interval, None);
        let config =
            Config::from_toml("[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"a:1\"").unwrap();
        assert_eq!(config.servers[0].stream_close_delay, None);
    }

    #[test]
    fn invalid_flush_interval_and_stream_close_delay_are_rejected() {
        let server = |options: &str| format!("[[server]]\nlisten = \"127.0.0.1:1\"\n{options}");
        let pool = |options: &str| {
            server(&format!(
                "forward = \"@pool\"\n[upstream.pool]\nbackends = [\"a:1\"]\n{options}"
            ))
        };
        for (text, expected) in [
            (
                pool("flush_interval = \"0s\""),
                "flush_interval must be greater than zero",
            ),
            (pool("flush_interval = -1"), "flush_interval"),
            (pool("flush_interval = \"soon\""), "invalid duration"),
            (
                server("stream_close_delay = \"later\"\nforward = \"127.0.0.1:2\""),
                "invalid duration",
            ),
            (
                server(
                    "protocol = \"tcp\"\nstream_close_delay = \"1s\"\nforward = \"127.0.0.1:2\"",
                ),
                "stream_close_delay only applies to http servers",
            ),
            (
                server(
                    "protocol = \"udp\"\nstream_close_delay = \"1s\"\nforward = \"127.0.0.1:2\"",
                ),
                "stream_close_delay only applies to http servers",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn http2_conns_per_host_parses_and_defaults_to_one() {
        let config = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:8000\"\nforward = \"@grpc\"\n\
             [upstream.grpc]\nbackends = [\"127.0.0.1:1\"]\nh2c = true\nhttp2_conns_per_host = 4",
        )
        .unwrap();
        assert_eq!(config.upstreams["grpc"].http2_conns_per_host, 4);
        assert_eq!(UpstreamGroup::default().http2_conns_per_host, 1);
    }

    #[test]
    fn invalid_connection_limit_buffers_and_body_size_are_rejected() {
        let forward = |options: &str| {
            format!(
                "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"@pool\"\n[upstream.pool]\nbackends = [\"127.0.0.1:2\"]\n{options}"
            )
        };
        let request = |options: &str| {
            format!("[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n{options}")
        };

        for (text, expected) in [
            (forward("max_conns_per_host = -1"), "max_conns_per_host"),
            (forward("http2_conns_per_host = 0"), "at least 1"),
            (forward("http2_conns_per_host = -1"), "http2_conns_per_host"),
            (
                forward("http2_conns_per_host = \"2\""),
                "http2_conns_per_host",
            ),
            (forward("max_conns_per_host = \"4\""), "max_conns_per_host"),
            (forward("request_buffers = \"4XB\""), "invalid size"),
            (forward("request_buffers = -1"), "request_buffers"),
            (forward("response_buffers = \"lots\""), "invalid size"),
            (forward("response_buffers = 1.5"), "response_buffers"),
            (
                request("request = { max_body_size = 0 }"),
                "greater than zero",
            ),
            (
                request("request = { max_body_size = \"1 mile\" }"),
                "invalid size",
            ),
            (
                request("protocol = \"tcp\"\nrequest = { max_body_size = 10 }"),
                "http",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn ip_allow_and_deny() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            ip_allow = ["10.0.0.0/8", "::1"]
            ip_deny = ["10.0.0.1"]
            match = [
                { uri = "/admin", ip_allow = ["127.0.0.1"], ip_deny = ["127.0.0.2/32"], forward = "127.0.0.1:8080" },
                { forward = "127.0.0.1:8080" },
            ]

            [[server]]
            listen = "127.0.0.1:5432"
            protocol = "tcp"
            ip_deny = ["192.168.0.0/16"]
            forward = "127.0.0.1:5433"

            [[server]]
            listen = "127.0.0.1:53"
            protocol = "udp"
            ip_allow = ["fd00::/8"]
            forward = "127.0.0.1:5353"
            "#,
        )
        .unwrap();

        let http = &config.servers[0];
        assert_eq!(http.ip_allow.len(), 2);
        assert_eq!(http.ip_deny, ["10.0.0.1/32".parse::<IpNet>().unwrap()]);
        assert_eq!(http.patterns[0].ip_allow.len(), 1);
        assert_eq!(http.patterns[0].ip_deny.len(), 1);
        assert!(http.patterns[1].ip_allow.is_empty());
        assert_eq!(config.servers[1].ip_deny.len(), 1);
        assert_eq!(config.servers[2].ip_allow.len(), 1);

        let invalid = |text: &str| Config::from_toml(text).unwrap_err().to_string();
        for (text, expected) in [
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n\
                 ip_allow = [\"10.0.0.0/33\"]",
                "invalid IP range",
            ),
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n\
                 ip_deny = [\"example.com\"]",
                "invalid IP range",
            ),
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n\
                 ip_deny = \"10.0.0.1\"",
                "invalid type",
            ),
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nmatch = [{ forward = \"127.0.0.1:2\", \
                 ip_allow = [\"10.0.0.1/8/8\"] }]",
                "invalid IP range",
            ),
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n\
                 match = [{ forward = \"127.0.0.1:2\", ip_allow = [\"10.0.0.0/8\"] }]",
                "ip_allow and ip_deny inside match entries only apply to http servers",
            ),
            (
                "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"udp\"\n\
                 match = [{ forward = \"127.0.0.1:2\", ip_deny = [\"10.0.0.0/8\"] }]",
                "ip_allow and ip_deny inside match entries only apply to http servers",
            ),
        ] {
            let message = invalid(text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn rate_limit_and_max_connections_per_ip() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            rate_limit = { rate = 100 }
            max_connections_per_ip = 20
            match = [
                { uri = "/login", rate_limit = { rate = 5, per = "1m", burst = 10, key = "header:X-Api-Key" }, forward = "127.0.0.1:8080" },
                { forward = "127.0.0.1:8080" },
            ]

            [[server]]
            listen = "127.0.0.1:5432"
            protocol = "tcp"
            max_connections_per_ip = 2
            forward = "127.0.0.1:5433"
            "#,
        )
        .unwrap();

        let http = &config.servers[0];
        let server_limit = http.rate_limit.as_ref().unwrap();
        assert_eq!(server_limit.rate, 100);
        assert_eq!(server_limit.per, Duration::from_secs(1));
        assert_eq!(server_limit.burst(), 100, "burst defaults to rate");
        assert_eq!(server_limit.key, RateLimitKey::ClientIp);
        assert_eq!(http.max_connections_per_ip, Some(20));
        let route_limit = http.patterns[0].rate_limit.as_ref().unwrap();
        assert_eq!(route_limit.per, Duration::from_secs(60));
        assert_eq!(route_limit.burst(), 10);
        assert_eq!(
            route_limit.key,
            RateLimitKey::Header(String::from("X-Api-Key"))
        );
        assert!(http.patterns[1].rate_limit.is_none());
        assert_eq!(config.servers[1].max_connections_per_ip, Some(2));

        // The serialized form parses back to the same limits.
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.servers[0].rate_limit, http.rate_limit);
        assert_eq!(
            again.servers[0].patterns[0].rate_limit,
            http.patterns[0].rate_limit
        );

        let invalid = |text: &str| Config::from_toml(text).unwrap_err().to_string();
        let http = "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n";
        for (text, expected) in [
            (
                format!("{http}rate_limit = {{ rate = 0 }}"),
                "rate must be greater than zero",
            ),
            (
                format!("{http}rate_limit = {{ burst = 5 }}"),
                "missing field `rate`",
            ),
            (
                format!("{http}rate_limit = {{ rate = 1, per = \"0s\" }}"),
                "per must be greater than zero",
            ),
            (
                format!("{http}rate_limit = {{ rate = 1, burst = 0 }}"),
                "burst must be greater than zero",
            ),
            (
                format!("{http}rate_limit = {{ rate = 1, key = \"cookie\" }}"),
                "invalid rate limit key",
            ),
            (
                format!("{http}rate_limit = {{ rate = 1, key = \"header:bad name\" }}"),
                "invalid header name",
            ),
            (
                format!("{http}rate_limit = {{ rate = 1, every = \"1s\" }}"),
                "unknown field",
            ),
            (format!("{http}rate_limit = 5"), "invalid type"),
            (
                format!("{http}max_connections_per_ip = 0"),
                "max_connections_per_ip must be greater than zero",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nmatch = [{ forward = \"127.0.0.1:2\", \
                     rate_limit = { rate = 0 } }]",
                ),
                "match[0]: rate_limit: rate must be greater than zero",
            ),
            (
                format!("{http}protocol = \"tcp\"\nrate_limit = {{ rate = 1 }}"),
                "rate_limit only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n\
                     match = [{ forward = \"127.0.0.1:2\", rate_limit = { rate = 1 } }]",
                ),
                "match[0]: rate_limit only applies to http servers",
            ),
            (
                format!("{http}protocol = \"udp\"\nmax_connections_per_ip = 5"),
                "max_connections_per_ip does not apply to udp servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nhost = \"a.test\"\n\
                     forward = \"127.0.0.1:2\"\nmax_connections_per_ip = 5\n\
                     [[server]]\nlisten = \"127.0.0.1:1\"\nhost = \"b.test\"\n\
                     forward = \"127.0.0.1:2\"",
                ),
                "must have the same max_connections_per_ip",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn basic_auth_and_forward_auth() {
        let hash = bcrypt::hash("secret", 4).unwrap();
        // What `htpasswd -nB` writes.
        let htpasswd = hash.replacen("$2b$", "$2y$", 1);
        let config = Config::from_toml(&format!(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            basic_auth = {{ users = {{ alice = "{hash}" }} }}
            forward_auth = {{ url = "http://127.0.0.1:9091/api/authz/forward-auth" }}
            match = [
                {{ uri = "/admin", basic_auth = {{ realm = "Admin", users = {{ root = "{htpasswd}" }} }}, forward = "127.0.0.1:8080" }},
                {{ uri = "/app", forward_auth = {{ url = "https://auth.example.com/verify?rd=1", copy_headers = ["Remote-User", "Remote-Groups"], timeout = "2s" }}, forward = "127.0.0.1:8080" }},
                {{ forward = "127.0.0.1:8080" }},
            ]
            "#
        ))
        .unwrap();

        let server = &config.servers[0];
        let basic = server.basic_auth.as_ref().unwrap();
        assert_eq!(basic.realm, "Restricted");
        assert!(basic.verify("alice", b"secret"));
        assert!(!basic.verify("alice", b"Secret"));
        assert!(!basic.verify("alice", b""));
        assert!(!basic.verify("bob", b"secret"), "unknown user");
        let forward = server.forward_auth.as_ref().unwrap();
        assert_eq!(forward.timeout, Duration::from_secs(10));
        assert!(forward.copy_headers.is_empty());

        let admin = server.patterns[0].basic_auth.as_ref().unwrap();
        assert_eq!(admin.realm, "Admin");
        assert!(admin.verify("root", b"secret"), "$2y$ hashes verify");
        assert!(!admin.verify("alice", b"secret"));
        assert!(server.patterns[0].forward_auth.is_none());
        let app = server.patterns[1].forward_auth.as_ref().unwrap();
        assert_eq!(app.url, "https://auth.example.com/verify?rd=1");
        assert_eq!(app.copy_headers, ["Remote-User", "Remote-Groups"]);
        assert_eq!(app.timeout, Duration::from_secs(2));
        assert!(server.patterns[2].basic_auth.is_none());

        // The serialized form parses back to the same settings.
        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.servers[0].basic_auth, server.basic_auth);
        assert_eq!(again.servers[0].forward_auth, server.forward_auth);
        assert_eq!(
            again.servers[0].patterns[0].basic_auth,
            server.patterns[0].basic_auth
        );
        assert_eq!(
            again.servers[0].patterns[1].forward_auth,
            server.patterns[1].forward_auth
        );

        let invalid = |text: &str| Config::from_toml(text).unwrap_err().to_string();
        let http = "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n";
        let users = format!("users = {{ alice = \"{hash}\" }}");
        for (text, expected) in [
            (
                format!("{http}basic_auth = {{ users = {{}} }}"),
                "basic_auth: users must not be empty",
            ),
            (
                format!("{http}basic_auth = {{ realm = \"x\" }}"),
                "missing field `users`",
            ),
            (
                format!("{http}basic_auth = {{ users = {{ alice = \"secret\" }} }}"),
                "user \"alice\": the password must be a bcrypt hash",
            ),
            (
                format!("{http}basic_auth = {{ users = {{ \"a:b\" = \"{hash}\" }} }}"),
                "must not be empty or contain a colon",
            ),
            (
                format!("{http}basic_auth = {{ users = {{ \"\" = \"{hash}\" }} }}"),
                "must not be empty or contain a colon",
            ),
            (
                format!("{http}basic_auth = {{ realm = 'a\"b', {users} }}"),
                "realm",
            ),
            (
                format!("{http}basic_auth = {{ {users}, file = \"x\" }}"),
                "unknown field",
            ),
            (
                format!("{http}protocol = \"tcp\"\nbasic_auth = {{ {users} }}"),
                "basic_auth only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nmatch = [{ forward = \
                     \"127.0.0.1:2\", basic_auth = { users = {} } }]",
                ),
                "match[0]: basic_auth: users must not be empty",
            ),
            (
                format!("{http}forward_auth = {{ timeout = \"1s\" }}"),
                "missing field `url`",
            ),
            (
                format!("{http}forward_auth = {{ url = \"127.0.0.1:9091\" }}"),
                "forward_auth: invalid url",
            ),
            (
                format!("{http}forward_auth = {{ url = \"ftp://auth/verify\" }}"),
                "forward_auth: invalid url",
            ),
            (
                format!("{http}forward_auth = {{ url = \"/verify\" }}"),
                "forward_auth: invalid url",
            ),
            (
                format!(
                    "{http}forward_auth = {{ url = \"http://auth\", copy_headers = [\"bad name\"] }}"
                ),
                "invalid header name \"bad name\" in copy_headers",
            ),
            (
                format!("{http}forward_auth = {{ url = \"http://auth\", timeout = \"0s\" }}"),
                "forward_auth: timeout must be greater than zero",
            ),
            (
                format!("{http}forward_auth = {{ url = \"http://auth\", address = \"x\" }}"),
                "unknown field",
            ),
            (
                format!("{http}protocol = \"tcp\"\nforward_auth = {{ url = \"http://auth\" }}"),
                "forward_auth only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nmatch = [{ forward = \
                     \"127.0.0.1:2\", forward_auth = { url = \"nope\" } }]",
                ),
                "match[0]: forward_auth: invalid url",
            ),
        ] {
            let message = invalid(&text);
            assert!(message.contains(expected), "{text}: {message}");
        }
    }

    #[test]
    fn security_headers_and_cors() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:8000"
            security_headers = "strict"
            cors = { origins = "https://app.example.com" }
            match = [
                { uri = "/api", security_headers = "off", cors = { origins = ["https://*.example.com", "http://localhost:3000"], methods = ["GET", "PURGE"], headers = ["X-Token"], expose_headers = "X-Total", credentials = true, max_age = "10m" }, forward = "127.0.0.1:8080" },
                { uri = "/docs", security_headers = "basic", forward = "127.0.0.1:8080" },
                { forward = "127.0.0.1:8080" },
            ]
            "#,
        )
        .unwrap();

        let server = &config.servers[0];
        assert_eq!(server.security_headers, Some(SecurityHeaders::Strict));
        let cors = server.cors.as_ref().unwrap();
        assert_eq!(cors.origins, ["https://app.example.com"]);
        assert_eq!(cors.methods, default::cors_methods());
        assert!(cors.headers.is_empty() && !cors.credentials && cors.max_age.is_none());

        let api = &server.patterns[0];
        assert_eq!(api.security_headers, Some(SecurityHeaders::Off));
        let api_cors = api.cors.as_ref().unwrap();
        assert_eq!(api_cors.methods, ["GET", "PURGE"]);
        assert_eq!(api_cors.headers, ["X-Token"]);
        assert_eq!(api_cors.expose_headers, ["X-Total"]);
        assert!(api_cors.credentials);
        assert_eq!(api_cors.max_age, Some(Duration::from_secs(600)));
        assert_eq!(
            server.patterns[1].security_headers,
            Some(SecurityHeaders::Basic)
        );
        assert!(server.patterns[2].security_headers.is_none());
        assert!(server.patterns[2].cors.is_none());

        let again = Config::from_toml(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.servers[0].security_headers, server.security_headers);
        assert_eq!(again.servers[0].cors, server.cors);
        assert_eq!(again.servers[0].patterns[0].cors, api.cors);
        assert_eq!(
            again.servers[0].patterns[0].security_headers,
            api.security_headers
        );

        let invalid = |text: &str| Config::from_toml(text).unwrap_err().to_string();
        let http = "[[server]]\nlisten = \"127.0.0.1:1\"\nforward = \"127.0.0.1:2\"\n";
        for (text, expected) in [
            (
                format!("{http}security_headers = \"paranoid\""),
                "unknown variant `paranoid`",
            ),
            (
                format!("{http}cors = {{ origins = [] }}"),
                "cors: origins must not be empty",
            ),
            (
                format!("{http}cors = {{ methods = [\"GET\"] }}"),
                "missing field `origins`",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", methods = [] }}"),
                "cors: methods must not be empty",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", credentials = true }}"),
                "cannot be combined with credentials",
            ),
            (
                format!("{http}cors = {{ origins = \"app.example.com\" }}"),
                "invalid origin \"app.example.com\"",
            ),
            (
                format!("{http}cors = {{ origins = \"https://app.example.com/\" }}"),
                "invalid origin",
            ),
            (
                format!("{http}cors = {{ origins = \"ftp://example.com\" }}"),
                "invalid origin",
            ),
            (
                format!("{http}cors = {{ origins = \"https://a.*.example.com\" }}"),
                "invalid origin",
            ),
            (
                format!("{http}cors = {{ origins = \"https://App.example.com\" }}"),
                "invalid origin",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", methods = [\"GE T\"] }}"),
                "cors: invalid method \"GE T\"",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", headers = [\"X Token\"] }}"),
                "cors: invalid header name \"X Token\"",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", expose_headers = [\"\"] }}"),
                "cors: invalid header name",
            ),
            (
                format!("{http}cors = {{ origins = \"*\", max_age = \"soon\" }}"),
                "max_age",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n\
                     forward = \"127.0.0.1:2\"\nsecurity_headers = \"basic\"",
                ),
                "security_headers only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"tcp\"\n\
                     forward = \"127.0.0.1:2\"\ncors = { origins = \"*\" }",
                ),
                "cors only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\nprotocol = \"udp\"\n\
                     match = [{ forward = \"127.0.0.1:2\", cors = { origins = \"*\" } }]",
                ),
                "match[0]: cors only applies to http servers",
            ),
            (
                String::from(
                    "[[server]]\nlisten = \"127.0.0.1:1\"\n\
                     match = [{ forward = \"127.0.0.1:2\", cors = { origins = [] } }]",
                ),
                "match[0]: cors: origins must not be empty",
            ),
        ] {
            let err = invalid(&text);
            assert!(err.contains(expected), "{text}: {err}");
        }
    }

    #[test]
    fn cors_origin_matching() {
        let cors = |origins: &[&str]| Cors {
            origins: origins.iter().map(|origin| String::from(*origin)).collect(),
            methods: default::cors_methods(),
            headers: Vec::new(),
            expose_headers: Vec::new(),
            credentials: false,
            max_age: None,
        };
        let any = cors(&["*"]);
        assert!(any.allows("https://anything.test"));
        assert!(any.allows("null"));

        let listed = cors(&["https://app.example.com", "https://*.example.org"]);
        assert!(listed.allows("https://app.example.com"));
        assert!(listed.allows("https://APP.example.com"));
        assert!(!listed.allows("http://app.example.com"), "scheme differs");
        assert!(
            !listed.allows("https://app.example.com:8443"),
            "port differs"
        );
        assert!(!listed.allows("https://app.example.com.evil.test"));
        assert!(listed.allows("https://a.example.org"));
        assert!(listed.allows("https://a.b.example.org"));
        assert!(!listed.allows("https://example.org"), "no subdomain");
        assert!(!listed.allows("https://evilexample.org"));
        assert!(
            !listed.allows("https://a.example.org:8443"),
            "port not in pattern"
        );
        assert!(!listed.allows("http://a.example.org"));
        assert!(!listed.allows("null"));
    }

    #[test]
    fn security_header_presets() {
        let names = |preset: SecurityHeaders, tls: bool| -> Vec<&str> {
            preset
                .headers(tls)
                .into_iter()
                .map(|(name, _)| name)
                .collect()
        };
        assert!(SecurityHeaders::Off.headers(true).is_empty());
        assert!(!names(SecurityHeaders::Basic, false).contains(&"strict-transport-security"));
        assert!(names(SecurityHeaders::Basic, true).contains(&"strict-transport-security"));
        assert!(names(SecurityHeaders::Strict, false).contains(&"content-security-policy"));
        assert!(!names(SecurityHeaders::Basic, false).contains(&"content-security-policy"));
        for preset in [SecurityHeaders::Basic, SecurityHeaders::Strict] {
            for (name, value) in preset.headers(true) {
                http::HeaderName::from_static(name);
                http::HeaderValue::from_static(value);
            }
        }
    }

    #[test]
    fn basic_auth_credentials_decoding() {
        // "alice:secret", "alice:a:b" (the password may contain colons), "alice:".
        assert_eq!(
            BasicAuth::decode_credentials("YWxpY2U6c2VjcmV0"),
            Some((String::from("alice"), b"secret".to_vec()))
        );
        assert_eq!(
            BasicAuth::decode_credentials("YWxpY2U6YTpi"),
            Some((String::from("alice"), b"a:b".to_vec()))
        );
        assert_eq!(
            BasicAuth::decode_credentials("YWxpY2U6"),
            Some((String::from("alice"), Vec::new()))
        );
        // "alice" without a colon, and something that is not base64.
        assert_eq!(BasicAuth::decode_credentials("YWxpY2U="), None);
        assert_eq!(BasicAuth::decode_credentials("alice:secret"), None);
    }
}
