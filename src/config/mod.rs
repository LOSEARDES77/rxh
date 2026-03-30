//! Structs and enums derived from the config file using [`serde`].

mod deser;

use std::{collections::HashMap, fmt::Debug, net::SocketAddr};

use deser::{BackendOption, ForwardOption};
use serde::{Deserialize, Serialize};

use crate::sched::{self, Scheduler};

/// This struct represents the entire configuration file, which describes a list
/// of servers and their particular configuration options. For example, this
/// configuration:
///
/// ```toml
/// [[server]]
///
/// listen = "127.0.0.1:8000"
/// forward = "127.0.0.1:8080"
///
/// [[server]]
///
/// listen = "127.0.0.1:9000"
/// serve = "/home/user/website"
/// ```
///
/// Should result in a [`Vec`] containing two [`Server`] elements after
/// deserializing.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    /// List of all servers.
    #[serde(rename = "server")]
    pub servers: Vec<Server>,
}

/// Description of a single server instance in the config file. The server
/// allows a "simple" pattern or multiple patterns. For example:
///
/// ```toml
/// # Simple pattern.
///
/// [[server]]
///
/// listen = "127.0.0.1:8000"
/// forward = "127.0.0.1:9000"
/// uri = "/api"
///
/// # Multiple patterns using "match".
///
/// [[server]]
///
/// listen = "128.0.01:8001"
///
/// match = [
///     { uri = "/front", serve = "/home/website" },
///     { uri = "/brack", forward = "127.0.0.1:9001" },
/// ]
/// ```
///
/// This is not provided by [`serde`], see [`deser`] module for implementation
/// details.
#[derive(Serialize, Debug, Clone)]
pub struct Server {
    /// Socket addresses where this server listens.
    pub listen: Vec<SocketAddr>,

    /// Patterns that this server should match against.
    #[serde(rename = "match")]
    pub patterns: Vec<Pattern>,

    /// Connections limit.
    #[serde(default = "default::max_connections")]
    pub max_connections: usize,

    /// Optional server name to show in logs and forwarded requests.
    pub name: Option<String>,

    /// Optional TLS configuration for incoming client connections.
    pub tls: Option<Tls>,

    /// Enables HTTP/2 support for incoming client connections.
    #[serde(default = "default::http2")]
    pub http2: bool,

    /// Log name inlcudes the IP address of the listening socket and also the
    /// optional name set by the user.
    #[serde(skip)]
    pub log_name: String,
}

/// This is a single element of a `match` list in the configuration of a server.
/// See [`Server`] and [`deser`] module.
///
/// ```toml
/// [[server]]
///
/// listen = "127.0.0.1:8000"
///
/// match = [
///     { uri = "/front", serve = "/home/website" },    # This is a Pattern
///     { uri = "/brack", forward = "127.0.0.1:9001" }, # This is another Pattern
/// ]
/// ```
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Pattern {
    /// URI prefix to match against.
    #[serde(default = "default::uri")]
    pub uri: String,

    /// Action to execute if this pattern matches the request.
    #[serde(flatten)]
    pub action: Action,

    /// Request header customization.
    #[serde(default)]
    pub request: RequestConfig,

    /// Response header customization.
    #[serde(default)]
    pub response: ResponseConfig,

    /// Optional cache configuration for this pattern.
    #[serde(default)]
    pub cache: Cache,
}

/// TLS certificates and keys for incoming connections.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Tls {
    /// Path to PEM encoded certificate chain.
    pub cert: String,

    /// Path to PEM encoded private key.
    pub key: String,
}

/// Per-pattern request customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RequestConfig {
    #[serde(default)]
    pub headers: RequestHeaders,
}

/// Per-pattern response customization.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ResponseConfig {
    #[serde(default)]
    pub headers: ResponseHeaders,
}

/// Request header settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct RequestHeaders {
    #[serde(default)]
    pub forwarded: ForwardedHeader,

    /// Replaces header values.
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Removes headers by name.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Response header settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ResponseHeaders {
    #[serde(default)]
    pub via: ViaHeader,

    #[serde(default)]
    pub server: ServerHeader,

    /// Replaces header values.
    #[serde(default)]
    pub set: HashMap<String, String>,

    /// Removes headers by name.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Customization for the forwarded header behavior.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ForwardedHeader {
    /// Appends to existing client-provided `Forwarded` values.
    #[serde(default = "default::true_value")]
    pub extend: bool,

    /// Overrides `by=` value.
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

/// Customization for the via header behavior.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ViaHeader {
    /// Appends this proxy as a new Via member when true.
    #[serde(default)]
    pub extend: bool,
}

/// Customization for the server header behavior.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ServerHeader {
    /// Whether to override the upstream Server header.
    #[serde(rename = "override", default = "default::true_value")]
    pub should_override: bool,

    /// Whether to append the RXH version in the Server value.
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

/// Minimal per-pattern cache configuration.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Cache {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default::cache_ttl_seconds")]
    pub ttl_seconds: u64,

    #[serde(default = "default::cache_max_entries")]
    pub max_entries: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            enabled: false,
            ttl_seconds: default::cache_ttl_seconds(),
            max_entries: default::cache_max_entries(),
        }
    }
}

/// One element in the "forward" list. This represents an upstream server and
/// when multiple of them are present load balancing has to be performed.
///
/// ```toml
/// /// [[server]]
///
/// listen = "127.0.0.1:8000"
/// forward = [
///     { address = "127.0.0.1:8080", weight = 1 }, # This is a Backend
///     { address = "127.0.0.1:8081", weight = 2 }, # This is another Backend
/// ]
/// ```
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(from = "BackendOption")]
pub struct Backend {
    /// Address of the upstream server.
    pub address: SocketAddr,

    /// Some algorithms such as WRR (Weighted Round Robin) require each server
    /// to define a weight. For example, a server with 4 cores can have a weight
    /// of 1 while a server with 8 cores can have a weight of 2.
    pub weight: usize,
}

/// Algorithm that should be used for load balancing. For now we only implement
/// WRR, so there's no point in specifying this, but the syntax is as follows:
///
/// ```toml
/// [[server]]
///
/// listen = "127.0.0.1:8000"
///
/// [[server.forward]]
///
/// algorithm = "WRR"
/// backends = [
///     { address = "127.0.0.1:8080", weight = 1 },
///     { address = "127.0.0.1:8081", weight = 2 },
/// ]
/// ```
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub enum Algorithm {
    #[serde(rename = "WRR")]
    Wrr,
}

/// Proxy specific configuration. This container is used to deserialize the
/// config:
///
/// ```toml
/// [[server]]
///
/// # This is the Forward struct
/// [[server.forward]]
///
/// algorithm = "WRR"
/// backends = [
///     { address = "127.0.0.1:8080", weight = 1 },
///     { address = "127.0.0.1:8081", weight = 2 },
/// ]
/// ```
///
/// But it's probably not necessary as we could store all this information
/// inside a [`Scheduler`]. We'll leave it here to match the config file and
/// keep it symmetric.
#[derive(Serialize, Deserialize)]
#[serde(from = "ForwardOption")]
pub struct Forward {
    /// Upstream servers.
    pub backends: Vec<Backend>,

    /// Algorithm used for load balancing.
    pub algorithm: Algorithm,

    /// Load balancing scheduler.
    #[serde(skip)]
    pub scheduler: Box<dyn Scheduler + Sync + Send>,
}

impl Debug for Forward {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Forward")
            .field("backends", &self.backends)
            .field("algorithm", &self.algorithm)
            .finish()
    }
}

impl Clone for Forward {
    fn clone(&self) -> Self {
        Self {
            backends: self.backends.clone(),
            algorithm: self.algorithm,
            scheduler: sched::make(self.algorithm, &self.backends),
        }
    }
}

/// Describes what should be done when a request matches a pattern. This
/// option is flattened to remove one level of identation in the config files.
/// Here's an example of simple actions:
///
/// ```toml
/// [[server]]
///
/// listen = "127.0.0.1:8000"
///
/// match = [
///     { uri = "/front", serve = "/home/website" },    # Serve action
///     { uri = "/brack", forward = "127.0.0.1:9001" }, # Forward action
/// ]
/// ```
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Forward the request to an upstream server or load balance between
    /// multiple of them.
    Forward(Forward),

    /// Serve static files from a root directory.
    Serve(String),
}

mod default {
    //! Default values for some configuration options.

    pub fn uri() -> String {
        String::from("/")
    }

    pub fn max_connections() -> usize {
        1024
    }

    pub fn cache_ttl_seconds() -> u64 {
        30
    }

    pub fn cache_max_entries() -> usize {
        1024
    }

    pub fn true_value() -> bool {
        true
    }

    pub fn http2() -> bool {
        true
    }
}
