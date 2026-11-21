//! Docker provider: turns the `rxh.*` labels of the running containers into
//! pools and servers, and follows the Docker events to keep them current.
//! Podman is the same through its Docker compatible API, except that a
//! container is reached at the host port it publishes, if any: the network
//! of rootless containers cannot be reached from the host. In Swarm mode the
//! running tasks of the services take the place of the containers, with the
//! labels of their service, and are listed every `refresh`: the events of the
//! tasks of other nodes do not reach this one.
//!
//! | Label             | Meaning                                                   |
//! |-------------------|-----------------------------------------------------------|
//! | `rxh.enable`      | `true` to route to the container                          |
//! | `rxh.http.host`   | Host names served on `[docker] listen`, comma separated   |
//! | `rxh.http.port`   | Container port of the HTTP route                          |
//! | `rxh.tcp.listen`  | Address of a TCP server for the container, `:5432`        |
//! | `rxh.tcp.port`    | Container port of the TCP route                           |
//! | `rxh.pool`        | Pool name, shared by the replicas of a service            |
//! | `rxh.policy`      | Load balancing policy of the pool                         |
//! | `rxh.groups`      | Groups of the server, comma separated                     |
//!
//! Each pool becomes an upstream group without static backends plus one
//! server named `docker-POOL`. The containers are its dynamic members, so a
//! replica that starts or stops only changes the members; the listeners are
//! rebuilt, like a reload, only when a pool appears, disappears or changes
//! its hosts, groups, address or policy. The hosts and groups of a pool are
//! those of all its containers.
//!
//! The service catalogs (`[consul]`, `[etcd]`, `[nomad]`, see the `catalog`
//! module) turn their service instances into [`Container`]s with the same
//! options, so they are planned here too, with servers named after their
//! section (`consul-POOL`). So does `[kubernetes]`, see the `kubernetes`
//! module.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Request, Response, header};
use hyper_util::rt::TokioIo;
use rxh_core::config::{
    Backend, CatalogKind, Config, ContainerProvider, Docker, Policy, Server, UpstreamGroup,
    is_group_name,
};
use rxh_core::matcher::HostPattern;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use crate::registry::Registry;

/// Running containers labelled `rxh.enable=true`.
const CONTAINERS: &str =
    "/containers/json?filters=%7B%22label%22%3A%5B%22rxh.enable%3Dtrue%22%5D%7D";

/// `start` and `die` events of the containers labelled `rxh.enable=true`.
const EVENTS: &str = "/events?filters=%7B%22type%22%3A%5B%22container%22%5D%2C%22event%22%3A%5B\
                      %22start%22%2C%22die%22%5D%2C%22label%22%3A%5B%22rxh.enable%3Dtrue%22%5D%7D";

/// Swarm services labelled `rxh.enable=true`.
const SERVICES: &str = "/services?filters=%7B%22label%22%3A%5B%22rxh.enable%3Dtrue%22%5D%7D";

/// Swarm tasks meant to run.
const TASKS: &str = "/tasks?filters=%7B%22desired-state%22%3A%5B%22running%22%5D%7D";

/// Every container of one provider, sent each time they may have changed.
pub(crate) type Updates = mpsc::Sender<(Provider, Vec<Container>)>;

/// Wait before connecting again to a Docker daemon that failed.
const RETRY: Duration = Duration::from_secs(2);

const LIST_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_LIST_BODY: usize = 16 * 1024 * 1024;

/// Provider a [`Container`] comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Provider {
    Docker,
    Catalog(CatalogKind),
}

impl Provider {
    /// Prefix of the log lines.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Docker => "Docker",
            Self::Catalog(CatalogKind::Consul) => "Consul",
            Self::Catalog(CatalogKind::Etcd) => "etcd",
            Self::Catalog(CatalogKind::Nomad) => "Nomad",
            Self::Catalog(CatalogKind::Kubernetes) => "Kubernetes",
        }
    }

    /// Prefix of the server names.
    fn name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Catalog(kind) => kind.section(),
        }
    }

    /// Where the HTTP servers of the routes listen, if the section is set.
    fn listen(self, config: &Config) -> Option<SocketAddr> {
        match self {
            Self::Docker => config.docker.as_ref().map(|docker| docker.listen),
            Self::Catalog(kind) => config.catalog(kind).map(|catalog| catalog.listen),
        }
    }

    /// What one of its [`Container`]s is, in messages.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            Self::Docker => "container",
            // Ingresses and HTTPRoutes are reported too, not only endpoints.
            Self::Catalog(CatalogKind::Kubernetes) => "object",
            Self::Catalog(_) => "instance",
        }
    }
}

/// A running container or service instance, as far as routing cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Container {
    pub(crate) source: Provider,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) labels: BTreeMap<String, String>,
    /// Address rxh reaches the container at, if it has one.
    pub(crate) address: Option<IpAddr>,
    /// TCP ports the container exposes, sorted.
    pub(crate) ports: Vec<u16>,
    /// Host addresses of the exposed ports, when they are to be preferred
    /// over `address` (Podman).
    pub(crate) published: BTreeMap<u16, SocketAddr>,
    /// Swarm or catalog service: the default pool.
    pub(crate) service: Option<String>,
    /// Why the provider cannot route this, reported like a bad label.
    pub(crate) rejected: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Listed {
    id: String,
    #[serde(default)]
    names: Vec<String>,
    labels: Option<BTreeMap<String, String>>,
    ports: Option<Vec<ListedPort>>,
    #[serde(default)]
    host_config: ListedHostConfig,
    #[serde(default)]
    network_settings: ListedNetworkSettings,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedPort {
    private_port: u16,
    public_port: Option<u16>,
    #[serde(rename = "IP")]
    ip: Option<String>,
    #[serde(rename = "Type")]
    protocol: String,
}

impl ListedPort {
    /// Where the host publishes the port; a wildcard is reached on loopback.
    fn published(&self) -> Option<SocketAddr> {
        let port = self.public_port.filter(|port| *port != 0)?;
        let ip: IpAddr = match self.ip.as_deref() {
            None | Some("" | "0.0.0.0") => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Some("::") => IpAddr::V6(Ipv6Addr::LOCALHOST),
            Some(ip) => ip.parse().ok()?,
        };
        Some(SocketAddr::new(ip, port))
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ListedHostConfig {
    #[serde(default)]
    network_mode: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct ListedNetworkSettings {
    networks: Option<BTreeMap<String, ListedNetwork>>,
}

#[derive(Deserialize)]
struct ListedNetwork {
    #[serde(rename = "IPAddress", default)]
    ip_address: String,
}

impl Listed {
    fn into_container(self, provider: ContainerProvider) -> Container {
        let listed = self;
        let address: Option<IpAddr> = if listed.host_config.network_mode == "host" {
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            // With several networks the first one by name wins.
            listed
                .network_settings
                .networks
                .unwrap_or_default()
                .values()
                .find_map(|network| network.ip_address.parse().ok())
        };
        let tcp: Vec<ListedPort> = listed
            .ports
            .unwrap_or_default()
            .into_iter()
            .filter(|port| port.protocol == "tcp")
            .collect();
        let mut ports: Vec<u16> = tcp.iter().map(|port| port.private_port).collect();
        ports.sort_unstable();
        ports.dedup();
        let mut published: BTreeMap<u16, SocketAddr> = BTreeMap::new();
        if provider == ContainerProvider::Podman {
            // The IPv4 entry comes first when a port is published twice.
            for port in &tcp {
                if let Some(address) = port.published() {
                    published.entry(port.private_port).or_insert(address);
                }
            }
        }
        let name = match listed.names.first() {
            Some(name) => name.trim_start_matches('/').to_owned(),
            None => listed.id.chars().take(12).collect(),
        };
        Container {
            source: Provider::Docker,
            id: listed.id,
            name,
            labels: listed.labels.unwrap_or_default(),
            address,
            ports,
            published,
            service: None,
            rejected: None,
        }
    }
}

/// Parses the body of `GET /containers/json` of a Docker or Podman API.
fn parse_containers(body: &[u8], provider: ContainerProvider) -> Result<Vec<Container>, String> {
    let listed: Vec<Listed> =
        serde_json::from_slice(body).map_err(|err| format!("container list: {err}"))?;
    Ok(listed
        .into_iter()
        .map(|listed| listed.into_container(provider))
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmService {
    #[serde(rename = "ID")]
    id: String,
    spec: SwarmServiceSpec,
    #[serde(default)]
    endpoint: SwarmEndpoint,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmServiceSpec {
    name: String,
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct SwarmEndpoint {
    ports: Option<Vec<SwarmPort>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmPort {
    #[serde(default)]
    protocol: String,
    target_port: u16,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmTask {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "ServiceID")]
    service_id: String,
    #[serde(default)]
    slot: u64,
    #[serde(rename = "NodeID", default)]
    node_id: String,
    status: SwarmTaskStatus,
    networks_attachments: Option<Vec<SwarmAttachment>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmTaskStatus {
    state: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmAttachment {
    network: SwarmNetwork,
    #[serde(default)]
    addresses: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmNetwork {
    spec: SwarmNetworkSpec,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SwarmNetworkSpec {
    name: String,
    #[serde(default)]
    ingress: bool,
}

/// Turns the bodies of `GET /services` and `GET /tasks` into one container
/// per running task of the listed services.
fn parse_swarm(services: &[u8], tasks: &[u8]) -> Result<Vec<Container>, String> {
    let services: Vec<SwarmService> =
        serde_json::from_slice(services).map_err(|err| format!("service list: {err}"))?;
    let tasks: Vec<SwarmTask> =
        serde_json::from_slice(tasks).map_err(|err| format!("task list: {err}"))?;
    let mut containers: Vec<Container> = Vec::new();
    for task in tasks {
        let Some(service) = services
            .iter()
            .find(|service| service.id == task.service_id)
        else {
            continue;
        };
        if task.status.state != "running" {
            continue;
        }
        let mut ports: Vec<u16> = match &service.endpoint.ports {
            Some(ports) => ports
                .iter()
                .filter(|port| port.protocol == "tcp")
                .map(|port| port.target_port)
                .collect(),
            None => Vec::new(),
        };
        ports.sort_unstable();
        ports.dedup();
        // The routing mesh (`ingress`) only carries published ports; with
        // several other networks the first one by name wins, as for Docker.
        let mut attachments: Vec<&SwarmAttachment> = task
            .networks_attachments
            .iter()
            .flatten()
            .filter(|attachment| !attachment.network.spec.ingress)
            .collect();
        attachments.sort_by(|a, b| a.network.spec.name.cmp(&b.network.spec.name));
        let address: Option<IpAddr> = attachments.iter().find_map(|attachment| {
            let first = attachment.addresses.first()?;
            let ip = first.split_once('/').map_or(first.as_str(), |(ip, _)| ip);
            ip.parse().ok()
        });
        let name = &service.spec.name;
        // Replicated tasks have a slot, global ones one task per node.
        let name = if task.slot > 0 {
            format!("{name}.{}", task.slot)
        } else {
            format!("{name}.{}", task.node_id)
        };
        containers.push(Container {
            source: Provider::Docker,
            id: task.id,
            name,
            labels: service.spec.labels.clone().unwrap_or_default(),
            address,
            ports,
            published: BTreeMap::new(),
            service: Some(service.spec.name.clone()),
            rejected: None,
        });
    }
    Ok(containers)
}

/// What the labels of one container ask for.
struct Target {
    pool: String,
    policy: Option<Policy>,
    hosts: Vec<String>,
    groups: Vec<String>,
    tcp_listen: Option<SocketAddr>,
    backend: Backend,
}

impl Container {
    fn target(&self) -> Result<Target, String> {
        if let Some(reason) = &self.rejected {
            return Err(reason.clone());
        }
        let mut pool: Option<&str> = None;
        let mut policy: Option<Policy> = None;
        let mut hosts: Vec<String> = Vec::new();
        let mut groups: Vec<String> = Vec::new();
        let mut tcp_listen: Option<SocketAddr> = None;
        let mut http_port: Option<u16> = None;
        let mut tcp_port: Option<u16> = None;
        let mut enabled = false;

        let port = |key: &str, value: &str| -> Result<Option<u16>, String> {
            match value.parse::<u16>() {
                Ok(port) if port != 0 => Ok(Some(port)),
                _ => Err(format!("{key} must be a port, got {value:?}")),
            }
        };
        for (key, value) in &self.labels {
            let Some(option) = key.strip_prefix("rxh.") else {
                continue;
            };
            match option {
                "enable" => enabled = value == "true",
                "pool" => pool = Some(value),
                "policy" => policy = Some(value.parse()?),
                "http.host" => {
                    hosts = value
                        .split(',')
                        .map(|host| host.trim().to_owned())
                        .filter(|host| !host.is_empty())
                        .collect();
                    // Not checked by `Config::validate`, and a bad pattern
                    // would fail every listener.
                    for host in &hosts {
                        HostPattern::new(host)?;
                    }
                }
                "groups" => {
                    groups.clear();
                    // Deduplicated here: a repeat would make the pool's server invalid.
                    for group in value.split(',').map(str::trim) {
                        if !group.is_empty() && !groups.iter().any(|known| known == group) {
                            groups.push(group.to_owned());
                        }
                    }
                    if let Some(group) = groups.iter().find(|group| !is_group_name(group)) {
                        return Err(format!(
                            "{key} has {group:?}, a group must match [a-z0-9][a-z0-9_-]* \
                             and have at most 64 bytes"
                        ));
                    }
                }
                "http.port" => http_port = port(key, value)?,
                "tcp.port" => tcp_port = port(key, value)?,
                "tcp.listen" => {
                    // Docker and Compose users write `:5432` for every address.
                    let address = match value.strip_prefix(':') {
                        Some(port) => format!("0.0.0.0:{port}"),
                        None => value.clone(),
                    };
                    tcp_listen = Some(address.parse().map_err(|_| {
                        format!("{key} must be an address such as :5432, got {value:?}")
                    })?);
                }
                _ => return Err(format!("unknown label {key}")),
            }
        }

        if !enabled {
            return Err(String::from("rxh.enable is not true"));
        }
        let port = match (hosts.is_empty(), tcp_listen) {
            (false, None) => http_port,
            (true, Some(_)) => tcp_port,
            (true, None) => return Err(String::from("needs rxh.http.host or rxh.tcp.listen")),
            (false, Some(_)) => {
                return Err(String::from(
                    "rxh.http.host and rxh.tcp.listen cannot be set on the same container",
                ));
            }
        };
        let port = match (port, self.ports.as_slice()) {
            (Some(port), _) => port,
            (None, [port]) => *port,
            (None, ports) => {
                return Err(format!(
                    "exposes {} TCP ports, set rxh.http.port or rxh.tcp.port",
                    ports.len()
                ));
            }
        };
        let backend = match self.published.get(&port) {
            Some(published) => *published,
            None => SocketAddr::new(
                self.address.ok_or(format!(
                    "has no IP address on a network and does not publish port {port}"
                ))?,
                port,
            ),
        };

        let pool = match pool {
            Some(pool) => pool.to_owned(),
            None => self.default_pool(),
        };
        let valid_name = !pool.is_empty()
            && pool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !valid_name {
            return Err(format!(
                "pool name {pool:?} may only have letters, digits, '-', '_' and '.'"
            ));
        }

        Ok(Target {
            pool,
            policy,
            hosts,
            groups,
            tcp_listen,
            backend: Backend::new(backend.to_string()),
        })
    }

    /// The Swarm service of a task, `PROJECT-SERVICE` for Compose
    /// containers, so the replicas of a service share a pool, else the
    /// container name.
    fn default_pool(&self) -> String {
        if let Some(service) = &self.service {
            return service.clone();
        }
        let label = |name: &str| self.labels.get(name);
        match (
            label("com.docker.compose.project"),
            label("com.docker.compose.service"),
        ) {
            (Some(project), Some(service)) => format!("{project}-{service}"),
            _ => self.name.clone(),
        }
    }
}

/// One pool made from containers and the server in front of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Route {
    source: Provider,
    pool: String,
    policy: Option<Policy>,
    /// Hosts of the HTTP server; unused with `tcp_listen`.
    hosts: Vec<String>,
    groups: Vec<String>,
    /// Address of the TCP server, or `None` for an HTTP route.
    tcp_listen: Option<SocketAddr>,
}

impl Route {
    fn add_to(&self, config: &mut Config) -> Result<(), String> {
        let http_listen = self
            .source
            .listen(config)
            .expect("routes are only planned for the sections set");
        let pool = &self.pool;
        if config.upstreams.contains_key(pool) {
            return Err(format!(
                "a pool named {pool} is already defined in the configuration"
            ));
        }
        let mut group = UpstreamGroup::default();
        if let Some(policy) = self.policy {
            group.policy = policy;
        }
        config.upstreams.insert(pool.clone(), group);

        let name = format!("{}-{pool}", self.source.name());
        let forward = format!("@{pool}");
        let groups = &self.groups;
        let server = match self.tcp_listen {
            Some(listen) => json!({
                "name": name, "listen": [listen], "protocol": "tcp", "forward": forward,
                "groups": groups,
            }),
            None => json!({
                "name": name, "listen": [http_listen], "host": self.hosts, "forward": forward,
                "groups": groups,
            }),
        };
        let server: Server = serde_json::from_value(server).map_err(|err| err.to_string())?;
        config.servers.push(server);
        Ok(())
    }
}

/// Routes and pool members for a set of containers.
#[derive(Debug, Default)]
pub(crate) struct Plan {
    /// Listeners depend on these only: a generation serves any [`Plan`] with
    /// the routes it was started with.
    pub(crate) routes: Vec<Route>,
    members: BTreeMap<String, Vec<Backend>>,
    /// Containers and pools left out, with the reason.
    pub(crate) ignored: Vec<String>,
}

impl Plan {
    /// Sets the containers as the members of their pools.
    pub(crate) fn sync(&self, registry: &Registry) {
        for (pool, backends) in &self.members {
            if let Some(group) = registry.get(pool) {
                group.pool.sync_dynamic(backends.clone());
            }
        }
    }
}

/// Logs the containers and pools in `ignored`, except the ones `known`
/// already reported.
pub(crate) fn log_ignored(ignored: &[String], known: &[String]) {
    for reason in ignored {
        if !known.contains(reason) {
            eprintln!("{reason}");
        }
    }
}

/// Plans the routes to `containers` on top of `config`, which is valid, and
/// returns `config` with the pools and servers of those routes. A container
/// or pool that cannot be routed, or that would make the configuration
/// invalid, is left out. Containers of a provider whose section is not set
/// are not routed.
pub(crate) fn plan(config: &Config, containers: &[Container]) -> (Config, Plan) {
    let mut effective = config.clone();
    let mut plan = Plan::default();

    let mut sorted: Vec<&Container> = containers
        .iter()
        .filter(|container| container.source.listen(config).is_some())
        .collect();
    sorted.sort_by(|a, b| (a.source, &a.name).cmp(&(b.source, &b.name)));
    for container in sorted {
        let source = container.source;
        let ignore = |reason: &str| {
            format!(
                "{} => ignored {} {}: {reason}",
                source.label(),
                source.noun(),
                container.name
            )
        };
        let target = match container.target() {
            Ok(target) => target,
            Err(reason) => {
                plan.ignored.push(ignore(&reason));
                continue;
            }
        };
        let Some(route) = plan
            .routes
            .iter_mut()
            .find(|route| route.pool == target.pool)
        else {
            plan.members
                .insert(target.pool.clone(), vec![target.backend]);
            plan.routes.push(Route {
                source,
                pool: target.pool,
                policy: target.policy,
                hosts: target.hosts,
                groups: target.groups,
                tcp_listen: target.tcp_listen,
            });
            continue;
        };
        if route.source != source {
            plan.ignored.push(ignore(&format!(
                "pool {} is already routed by {}",
                route.pool,
                route.source.label()
            )));
            continue;
        }
        if route.policy != target.policy || route.tcp_listen != target.tcp_listen {
            plan.ignored.push(ignore(&format!(
                "rxh.policy or rxh.tcp.listen differ from another {} of pool {}",
                source.noun(),
                route.pool
            )));
            continue;
        }
        for host in target.hosts {
            if !route.hosts.contains(&host) {
                route.hosts.push(host);
            }
        }
        for group in target.groups {
            if !route.groups.contains(&group) {
                route.groups.push(group);
            }
        }
        plan.members
            .entry(target.pool)
            .or_default()
            .push(target.backend);
    }

    let Plan {
        routes,
        members,
        ignored,
    } = &mut plan;
    routes.retain(|route| {
        let mut candidate = effective.clone();
        let outcome = route
            .add_to(&mut candidate)
            .and_then(|()| candidate.validate().map_err(|err| err.to_string()));
        match outcome {
            Ok(()) => {
                effective = candidate;
                true
            }
            Err(reason) => {
                ignored.push(format!(
                    "{} => ignored pool {}: {reason}",
                    route.source.label(),
                    route.pool
                ));
                members.remove(&route.pool);
                false
            }
        }
    });
    (effective, plan)
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "Action")]
    action: String,
    #[serde(rename = "Actor")]
    actor: Actor,
}

#[derive(Deserialize)]
struct Actor {
    #[serde(rename = "ID")]
    id: String,
}

/// Sends the routable containers of the engine `settings` point at to
/// `updates`: once after connecting, then after every container that starts
/// or dies, or every `refresh` for Swarm. While the engine cannot be reached
/// nothing is sent, so the last containers stay routed, and the connection is
/// retried forever.
pub(crate) async fn watch(settings: Docker, updates: Updates) {
    let endpoint = settings.endpoint();
    let mut last_error = String::new();
    loop {
        let outcome = match settings.provider {
            ContainerProvider::Swarm => poll_swarm(endpoint, settings.refresh, &updates).await,
            provider => follow(endpoint, provider, &updates).await,
        };
        match outcome {
            Ok(()) => return,
            Err(reason) => {
                if reason != last_error {
                    eprintln!("Docker => {endpoint}: {reason}, retrying every {RETRY:?}");
                    last_error = reason;
                }
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Returns `Ok` once nothing listens to `updates` anymore.
async fn poll_swarm(endpoint: &str, refresh: Duration, updates: &Updates) -> Result<(), String> {
    loop {
        let services = list(endpoint, SERVICES, "service list").await?;
        let tasks = list(endpoint, TASKS, "task list").await?;
        let containers = parse_swarm(&services, &tasks)?;
        if updates.send((Provider::Docker, containers)).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(refresh).await;
    }
}

/// Returns `Ok` once nothing listens to `updates` anymore.
async fn follow(
    endpoint: &str,
    provider: ContainerProvider,
    updates: &Updates,
) -> Result<(), String> {
    // Subscribed before listing, so a container that starts in between is
    // listed again instead of missed.
    let mut events = send(endpoint, docker_request(EVENTS)).await?.into_body();
    let mut buffered: Vec<u8> = Vec::new();
    // Docker can announce a death while it still lists the container as
    // running: those are left out until they start again.
    let mut dead: BTreeSet<String> = BTreeSet::new();
    loop {
        let body = list(endpoint, CONTAINERS, "container list").await?;
        let mut containers = parse_containers(&body, provider)?;
        containers.retain(|container| !dead.contains(&container.id));
        if updates.send((Provider::Docker, containers)).await.is_err() {
            return Ok(());
        }

        let line = next_line(&mut events, &mut buffered).await?;
        let event: Event =
            serde_json::from_slice(&line).map_err(|err| format!("event stream: {err}"))?;
        if event.action == "die" {
            dead.insert(event.actor.id);
        } else {
            dead.remove(&event.actor.id);
        }
    }
}

/// Body of the list at `path`, `what` naming it in errors.
async fn list(endpoint: &str, path: &str, what: &str) -> Result<Bytes, String> {
    fetch(endpoint, docker_request(path), what).await
}

fn docker_request(path: &str) -> Request<Full<Bytes>> {
    Request::get(path)
        .header(header::HOST, "docker")
        .body(Full::default())
        .expect("static request parts are valid")
}

/// Body of the answer to `request`, `what` naming it in errors. `endpoint`
/// is `unix:///path` or `tcp://host:port`.
pub(crate) async fn fetch(
    endpoint: &str,
    request: Request<Full<Bytes>>,
    what: &str,
) -> Result<Bytes, String> {
    tokio::time::timeout(LIST_TIMEOUT, async {
        Limited::new(send(endpoint, request).await?.into_body(), MAX_LIST_BODY)
            .collect()
            .await
            .map(|body| body.to_bytes())
            .map_err(|err| format!("{what}: {err}"))
    })
    .await
    .map_err(|_| format!("{what}: timeout"))?
}

/// Next line of the event stream, which has one JSON document per line.
async fn next_line(events: &mut Incoming, buffered: &mut Vec<u8>) -> Result<Vec<u8>, String> {
    loop {
        if let Some(end) = buffered.iter().position(|byte| *byte == b'\n') {
            let rest = buffered.split_off(end + 1);
            let line = std::mem::replace(buffered, rest);
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Ok(line);
        }
        match events.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    buffered.extend_from_slice(data);
                }
            }
            Some(Err(err)) => return Err(format!("event stream: {err}")),
            None => return Err(String::from("event stream closed")),
        }
    }
}

async fn send(endpoint: &str, request: Request<Full<Bytes>>) -> Result<Response<Incoming>, String> {
    let mut sender = connect(endpoint)
        .await
        .map_err(|err| format!("connect: {err}"))?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("API answered {}", response.status()));
    }
    Ok(response)
}

async fn connect(endpoint: &str) -> io::Result<SendRequest<Full<Bytes>>> {
    match endpoint.split_once("://") {
        #[cfg(unix)]
        Some(("unix", path)) => handshake(tokio::net::UnixStream::connect(path).await?).await,
        Some(("tcp", address)) => handshake(TcpStream::connect(address).await?).await,
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported endpoint",
        )),
    }
}

async fn handshake<S>(stream: S) -> io::Result<SendRequest<Full<Bytes>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(io::Error::other)?;
    tokio::task::spawn(async move {
        let _ = connection.await;
    });
    Ok(sender)
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Protocol;

    use super::*;

    fn container(name: &str, address: &str, ports: &[u16], labels: &[(&str, &str)]) -> Container {
        let mut labels: BTreeMap<String, String> = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        labels.insert("rxh.enable".into(), "true".into());
        Container {
            source: Provider::Docker,
            id: format!("id-{name}"),
            name: name.to_owned(),
            labels,
            address: address.parse().ok(),
            ports: ports.to_vec(),
            published: BTreeMap::new(),
            service: None,
            rejected: None,
        }
    }

    fn config(extra: &str) -> Config {
        Config::from_toml(&format!("[docker]\nlisten = \"127.0.0.1:8080\"\n{extra}")).unwrap()
    }

    fn members<'a>(plan: &'a Plan, pool: &str) -> Vec<&'a str> {
        plan.members[pool]
            .iter()
            .map(|backend| backend.address.as_str())
            .collect()
    }

    #[test]
    fn labels_become_pools_and_servers() {
        let containers = [
            container(
                "web-2",
                "172.18.0.3",
                &[80, 443],
                &[
                    ("rxh.http.host", "app.example.com, www.example.com"),
                    ("rxh.http.port", "8080"),
                    ("rxh.pool", "web"),
                    ("rxh.policy", "least_conn"),
                    ("maintainer", "someone"),
                ],
            ),
            container(
                "web-1",
                "172.18.0.2",
                &[8080],
                &[
                    ("rxh.http.host", "app.example.com"),
                    ("rxh.pool", "web"),
                    ("rxh.policy", "least_conn"),
                ],
            ),
            container("db", "172.18.0.9", &[5432], &[("rxh.tcp.listen", ":5432")]),
        ];
        let (effective, plan) = plan(&config(""), &containers);
        assert_eq!(plan.ignored, [""; 0]);

        assert_eq!(
            members(&plan, "web"),
            ["172.18.0.2:8080", "172.18.0.3:8080"]
        );
        assert_eq!(members(&plan, "db"), ["172.18.0.9:5432"]);
        assert_eq!(effective.upstreams["web"].policy, Policy::LeastConn);
        assert!(effective.upstreams["web"].backends.is_empty());

        let db = &effective.servers[0];
        assert_eq!(db.name.as_deref(), Some("docker-db"));
        assert_eq!(db.protocol, Protocol::Tcp);
        assert_eq!(db.listen, ["0.0.0.0:5432".parse().unwrap()]);

        let web = &effective.servers[1];
        assert_eq!(web.name.as_deref(), Some("docker-web"));
        assert_eq!(web.protocol, Protocol::Http);
        assert_eq!(web.listen, ["127.0.0.1:8080".parse().unwrap()]);
        assert_eq!(web.host, ["app.example.com", "www.example.com"]);
        effective.validate().unwrap();
    }

    #[test]
    fn docker_groups_label_sets_the_groups() {
        let containers = [
            container(
                "web-1",
                "10.0.0.1",
                &[80],
                &[
                    ("rxh.http.host", "app.test"),
                    ("rxh.pool", "web"),
                    ("rxh.groups", " web , customers,"),
                ],
            ),
            container(
                "web-2",
                "10.0.0.2",
                &[80],
                &[
                    ("rxh.http.host", "app.test"),
                    ("rxh.pool", "web"),
                    ("rxh.groups", "web,ops"),
                ],
            ),
            container(
                "db",
                "10.0.0.9",
                &[5432],
                &[("rxh.tcp.listen", ":5432"), ("rxh.groups", "data")],
            ),
            container("bare", "10.0.0.5", &[80], &[("rxh.http.host", "bare.test")]),
        ];
        let (effective, planned) = plan(&config(""), &containers);
        assert_eq!(planned.ignored, [""; 0]);
        let groups = |name: &str| -> &[String] {
            let server = effective
                .servers
                .iter()
                .find(|server| server.name.as_deref() == Some(name));
            &server.expect("the server is planned").groups
        };
        // The groups of a pool are those of all its replicas.
        assert_eq!(groups("docker-web"), ["web", "customers", "ops"]);
        assert_eq!(groups("docker-db"), ["data"]);
        assert!(groups("docker-bare").is_empty());

        // A change of groups is a change of route: the listeners are rebuilt.
        let regrouped = container(
            "db",
            "10.0.0.9",
            &[5432],
            &[("rxh.tcp.listen", ":5432"), ("rxh.groups", "other")],
        );
        let db = plan(&config(""), &containers[2..3]).1;
        assert_ne!(db.routes, plan(&config(""), &[regrouped]).1.routes);
    }

    #[test]
    fn docker_groups_label_with_an_invalid_name_is_reported() {
        for value in ["Web", "web,-x", "a b", &"x".repeat(65)] {
            let bad = container(
                "bad",
                "10.0.0.1",
                &[80],
                &[("rxh.http.host", "bad.test"), ("rxh.groups", value)],
            );
            let good = container("good", "10.0.0.2", &[80], &[("rxh.http.host", "good.test")]);
            let (effective, planned) = plan(&config(""), &[bad, good]);
            assert_eq!(planned.ignored.len(), 1, "{value}: {:?}", planned.ignored);
            assert!(
                planned.ignored[0].starts_with("Docker => ignored container bad: rxh.groups has"),
                "{value}: {:?}",
                planned.ignored
            );
            let names: Vec<Option<&str>> = effective
                .servers
                .iter()
                .map(|server| server.name.as_deref())
                .collect();
            assert_eq!(names, [Some("docker-good")], "{value}");
        }

        // A group repeated in a label counts once, and does not take down the
        // pool it shares with others, whichever replica is planned first.
        let replica = |name: &str, address: &str, groups: &str| {
            container(
                name,
                address,
                &[80],
                &[
                    ("rxh.http.host", "twice.test"),
                    ("rxh.pool", "twice"),
                    ("rxh.groups", groups),
                ],
            )
        };
        let containers = [
            replica("b", "10.0.0.2", "ops"),
            replica("a", "10.0.0.1", "web, web,ops"),
        ];
        let (effective, planned) = plan(&config(""), &containers);
        assert_eq!(planned.ignored, [""; 0]);
        assert_eq!(effective.servers[0].groups, ["web", "ops"]);
        assert_eq!(members(&planned, "twice"), ["10.0.0.1:80", "10.0.0.2:80"]);
    }

    #[test]
    fn replicas_change_members_but_not_routes() {
        let replica = |name: &str, address: &str| {
            container(
                name,
                address,
                &[80],
                &[("rxh.http.host", "app.test"), ("rxh.pool", "app")],
            )
        };
        let one = plan(&config(""), &[replica("a", "10.0.0.1")]).1;
        let two = plan(
            &config(""),
            &[replica("b", "10.0.0.2"), replica("a", "10.0.0.1")],
        )
        .1;
        assert_eq!(one.routes, two.routes);
        assert_eq!(members(&two, "app"), ["10.0.0.1:80", "10.0.0.2:80"]);

        let other_host = container("a", "10.0.0.1", &[80], &[("rxh.http.host", "b.test")]);
        assert_ne!(one.routes, plan(&config(""), &[other_host]).1.routes);
    }

    #[test]
    fn compose_services_share_a_pool_by_default() {
        let replica = |name: &str, address: &str| {
            container(
                name,
                address,
                &[80],
                &[
                    ("rxh.http.host", "app.test"),
                    ("com.docker.compose.project", "shop"),
                    ("com.docker.compose.service", "web"),
                ],
            )
        };
        let plan = plan(
            &config(""),
            &[
                replica("shop-web-1", "10.0.0.1"),
                replica("shop-web-2", "10.0.0.2"),
            ],
        )
        .1;
        assert_eq!(members(&plan, "shop-web").len(), 2);

        let alone = container("alone", "10.0.0.3", &[80], &[("rxh.http.host", "x.test")]);
        assert_eq!(super::plan(&config(""), &[alone]).1.routes[0].pool, "alone");
    }

    #[test]
    fn containers_that_cannot_be_routed_are_ignored_with_a_reason() {
        let http = ("rxh.http.host", "app.test");
        let ignored = |container: Container, extra: &str| -> String {
            let (effective, plan) = plan(&config(extra), &[container]);
            assert!(
                plan.routes.is_empty() && plan.members.is_empty(),
                "{plan:?}"
            );
            assert_eq!(effective.servers.len(), config(extra).servers.len());
            effective.validate().unwrap();
            plan.ignored.join("\n")
        };

        let mut disabled = container("c", "10.0.0.1", &[80], &[http]);
        disabled.labels.insert("rxh.enable".into(), "false".into());
        assert!(ignored(disabled, "").contains("rxh.enable is not true"));

        for (labels, ports, reason) in [
            (vec![], vec![80], "needs rxh.http.host or rxh.tcp.listen"),
            (
                vec![http, ("rxh.tcp.listen", ":1")],
                vec![80],
                "cannot be set on the same",
            ),
            (vec![http], vec![80, 81], "exposes 2 TCP ports"),
            (vec![http], vec![], "exposes 0 TCP ports"),
            (
                vec![http, ("rxh.http.port", "http")],
                vec![80],
                "rxh.http.port must be a port",
            ),
            (
                vec![http, ("rxh.http.port", "0")],
                vec![80],
                "rxh.http.port must be a port",
            ),
            (
                vec![("rxh.tcp.listen", "5432")],
                vec![80],
                "rxh.tcp.listen must be an address",
            ),
            (vec![http, ("rxh.policy", "fastest")], vec![80], "fastest"),
            (
                vec![http, ("rxh.htpp.port", "80")],
                vec![80],
                "unknown label rxh.htpp.port",
            ),
            (
                vec![http, ("rxh.pool", "a b")],
                vec![80],
                "pool name \"a b\"",
            ),
            // Rejected by the validation of the configuration.
            (vec![http, ("rxh.policy", "header")], vec![80], "pool c:"),
            (
                vec![("rxh.http.host", "a.test, *")],
                vec![80],
                "invalid host \"*\"",
            ),
            (
                vec![("rxh.tcp.listen", ":1"), ("rxh.policy", "uri_hash")],
                vec![80],
                "pool c:",
            ),
        ] {
            let message = ignored(container("c", "10.0.0.1", &ports, &labels), "");
            assert!(message.contains(reason), "{labels:?}: {message}");
        }

        let message = ignored(container("c", "", &[80], &[http]), "");
        assert!(message.contains("has no IP address"), "{message}");

        let file = "[upstream.c]\nbackends = [\"127.0.0.1:1\"]\n\
                    [[server]]\nname = \"docker-d\"\nlisten = \"127.0.0.1:1\"\nforward = \"@c\"\n";
        let message = ignored(container("c", "10.0.0.1", &[80], &[http]), file);
        assert!(
            message.contains("a pool named c is already defined"),
            "{message}"
        );
        let message = ignored(container("d", "10.0.0.1", &[80], &[http]), file);
        assert!(message.contains("docker-d"), "{message}");
    }

    #[test]
    fn a_bad_container_does_not_take_the_others_down() {
        let containers = [
            container("bad", "10.0.0.1", &[], &[("rxh.http.host", "bad.test")]),
            container("good", "10.0.0.2", &[80], &[("rxh.http.host", "good.test")]),
            container(
                "odd",
                "10.0.0.3",
                &[80],
                &[
                    ("rxh.http.host", "good.test"),
                    ("rxh.pool", "good"),
                    ("rxh.policy", "random"),
                ],
            ),
        ];
        let (effective, plan) = plan(&config(""), &containers);
        assert_eq!(plan.routes.len(), 1);
        assert_eq!(members(&plan, "good"), ["10.0.0.2:80"]);
        assert_eq!(plan.ignored.len(), 2, "{:?}", plan.ignored);
        assert!(plan.ignored[1].contains("differ from another container of pool good"));
        assert_eq!(effective.servers.len(), 1);
    }

    #[test]
    fn nothing_is_routed_without_the_docker_section() {
        let containers = [container(
            "a",
            "10.0.0.1",
            &[80],
            &[("rxh.http.host", "a.test")],
        )];
        let (effective, plan) = plan(&Config::default(), &containers);
        assert!(plan.routes.is_empty() && effective.servers.is_empty());
    }

    #[test]
    fn container_list_of_the_docker_api_is_parsed() {
        let body = br#"[
          {"Id":"0123456789abcdef","Names":["/shop-web-1"],"Image":"nginx","State":"running",
           "Labels":{"rxh.enable":"true","rxh.http.host":"app.test"},
           "Ports":[{"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
                    {"IP":"::","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
                    {"PrivatePort":53,"Type":"udp"}],
           "HostConfig":{"NetworkMode":"shop_default"},
           "NetworkSettings":{"Networks":{
             "shop_default":{"IPAddress":"172.20.0.5","Gateway":"172.20.0.1"},
             "a_pending":{"IPAddress":""}}}},
          {"Id":"fedcba9876543210","Names":[],"Labels":null,"Ports":null,
           "HostConfig":{"NetworkMode":"host"},"NetworkSettings":{"Networks":{"host":{"IPAddress":""}}}},
          {"Id":"none"}
        ]"#;
        let containers = parse_containers(body, ContainerProvider::Docker).unwrap();
        assert_eq!(containers[0].name, "shop-web-1");
        assert_eq!(containers[0].address, "172.20.0.5".parse().ok());
        assert_eq!(containers[0].ports, [80]);
        assert_eq!(containers[0].labels["rxh.http.host"], "app.test");
        assert_eq!(containers[1].name, "fedcba987654");
        assert_eq!(containers[1].address, "127.0.0.1".parse().ok());
        assert_eq!(containers[2].address, None);

        assert!(parse_containers(b"{\"message\":\"no\"}", ContainerProvider::Docker).is_err());
        // Docker reaches containers on their network, published or not.
        assert!(
            containers
                .iter()
                .all(|container| container.published.is_empty())
        );
    }

    #[test]
    fn podman_containers_are_reached_at_their_published_ports() {
        // Rootless Podman with pasta: no address on a network.
        let body = br#"[
          {"Id":"a1","Names":["web"],
           "Labels":{"rxh.enable":"true","rxh.http.host":"app.test"},
           "Ports":[{"IP":"0.0.0.0","PrivatePort":8000,"PublicPort":41669,"Type":"tcp"},
                    {"IP":"::","PrivatePort":8000,"PublicPort":41669,"Type":"tcp"},
                    {"IP":"10.1.2.3","PrivatePort":9000,"PublicPort":9001,"Type":"tcp"},
                    {"IP":"::","PrivatePort":9100,"PublicPort":9101,"Type":"tcp"},
                    {"IP":"0.0.0.0","PrivatePort":53,"PublicPort":5353,"Type":"udp"},
                    {"PrivatePort":9200,"Type":"tcp"}],
           "HostConfig":{"NetworkMode":"pasta"},"NetworkSettings":{"Networks":{}}},
          {"Id":"b2","Names":["db"],
           "Labels":{"rxh.enable":"true","rxh.tcp.listen":":5432"},
           "Ports":[{"PrivatePort":5432,"Type":"tcp"}],
           "HostConfig":{"NetworkMode":"bridge"},
           "NetworkSettings":{"Networks":{"podman":{"IPAddress":"10.88.0.4"}}}}
        ]"#;
        let containers = parse_containers(body, ContainerProvider::Podman).unwrap();
        let published: Vec<(u16, String)> = containers[0]
            .published
            .iter()
            .map(|(port, address)| (*port, address.to_string()))
            .collect();
        assert_eq!(
            published,
            [
                (8000, "127.0.0.1:41669".into()),
                (9000, "10.1.2.3:9001".into()),
                (9100, "[::1]:9101".into()),
            ]
        );
        assert_eq!(containers[0].address, None);

        let mut containers = containers;
        containers[0]
            .labels
            .insert("rxh.http.port".into(), "8000".into());
        let (_, routed) = plan(&config(""), &containers);
        assert_eq!(routed.ignored, [""; 0]);
        assert_eq!(members(&routed, "web"), ["127.0.0.1:41669"]);
        // Not published: the address on the network.
        assert_eq!(members(&routed, "db"), ["10.88.0.4:5432"]);

        // A port that is neither published nor reachable on a network.
        containers[0]
            .labels
            .insert("rxh.http.port".into(), "9200".into());
        let (_, routed) = plan(&config(""), &containers[..1]);
        assert!(
            routed.ignored[0]
                .contains("has no IP address on a network and does not publish port 9200"),
            "{:?}",
            routed.ignored
        );

        // The same list from Docker ignores the published ports.
        let docker = parse_containers(body, ContainerProvider::Docker).unwrap();
        assert!(docker[0].published.is_empty());
    }

    #[test]
    fn swarm_tasks_are_the_containers_of_their_service() {
        let services = br#"[
          {"ID":"s1","Spec":{"Name":"shop_web","Labels":{"rxh.enable":"true","rxh.http.host":"shop.test"}},
           "Endpoint":{"Ports":[{"Protocol":"tcp","TargetPort":80,"PublishedPort":8080},
                                {"Protocol":"udp","TargetPort":53,"PublishedPort":53}]}},
          {"ID":"s2","Spec":{"Name":"metrics","Labels":{"rxh.enable":"true","rxh.http.host":"metrics.test",
                                                        "rxh.http.port":"9100"}},
           "Endpoint":{}}
        ]"#;
        let attachments = |address: &str| {
            format!(
                r#"[{{"Network":{{"Spec":{{"Name":"ingress","Ingress":true}}}},"Addresses":["10.0.0.9/24"]}},
                    {{"Network":{{"Spec":{{"Name":"shop_default"}}}},"Addresses":["{address}/24"]}},
                    {{"Network":{{"Spec":{{"Name":"z_other"}}}},"Addresses":["10.9.9.9/24"]}}]"#
            )
        };
        let tasks = format!(
            r#"[
              {{"ID":"t1","ServiceID":"s1","Slot":1,"NodeID":"n1","Status":{{"State":"running"}},
                "NetworksAttachments":{}}},
              {{"ID":"t2","ServiceID":"s1","Slot":2,"NodeID":"n2","Status":{{"State":"running"}},
                "NetworksAttachments":{}}},
              {{"ID":"t3","ServiceID":"s1","Slot":3,"NodeID":"n2","Status":{{"State":"starting"}},
                "NetworksAttachments":{}}},
              {{"ID":"t4","ServiceID":"s2","NodeID":"n1","Status":{{"State":"running"}},
                "NetworksAttachments":[{{"Network":{{"Spec":{{"Name":"host"}}}},"Addresses":["10.2.0.1/16"]}}]}},
              {{"ID":"t5","ServiceID":"unlabelled","Slot":1,"Status":{{"State":"running"}}}}
            ]"#,
            attachments("10.0.1.5"),
            attachments("10.0.1.6"),
            attachments("10.0.1.7"),
        );
        let containers = parse_swarm(services, tasks.as_bytes()).unwrap();
        let names: Vec<&str> = containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["shop_web.1", "shop_web.2", "metrics.n1"]);
        assert_eq!(containers[0].ports, [80]);
        assert!(containers[2].ports.is_empty());

        let (effective, routed) = plan(&config(""), &containers);
        assert_eq!(routed.ignored, [""; 0]);
        assert_eq!(members(&routed, "shop_web"), ["10.0.1.5:80", "10.0.1.6:80"]);
        assert_eq!(members(&routed, "metrics"), ["10.2.0.1:9100"]);
        assert_eq!(effective.servers.len(), 2);

        assert!(
            parse_swarm(b"{}", b"[]")
                .unwrap_err()
                .contains("service list")
        );
        assert!(
            parse_swarm(b"[]", b"[{}]")
                .unwrap_err()
                .contains("task list")
        );
    }
}
