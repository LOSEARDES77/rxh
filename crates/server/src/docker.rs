//! Docker provider: turns the `rxh.*` labels of the running containers into
//! pools and servers, and follows the Docker events to keep them current.
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
//!
//! Each pool becomes an upstream group without static backends plus one
//! server named `docker-POOL`. The containers are its dynamic members, so a
//! replica that starts or stops only changes the members; the listeners are
//! rebuilt, like a reload, only when a pool appears, disappears or changes
//! its hosts, address or policy.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Request, Response, header};
use hyper_util::rt::TokioIo;
use rxh_core::config::{Backend, Config, Policy, Server, UpstreamGroup};
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

/// Wait before connecting again to a Docker daemon that failed.
const RETRY: Duration = Duration::from_secs(2);

const LIST_TIMEOUT: Duration = Duration::from_secs(10);

const MAX_LIST_BODY: usize = 16 * 1024 * 1024;

/// A running container, as far as routing cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Container {
    id: String,
    name: String,
    labels: BTreeMap<String, String>,
    /// Address rxh reaches the container at, if it has one.
    address: Option<IpAddr>,
    /// TCP ports the container exposes, sorted.
    ports: Vec<u16>,
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
    #[serde(rename = "Type")]
    protocol: String,
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

impl From<Listed> for Container {
    fn from(listed: Listed) -> Self {
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
        let mut ports: Vec<u16> = listed
            .ports
            .unwrap_or_default()
            .iter()
            .filter(|port| port.protocol == "tcp")
            .map(|port| port.private_port)
            .collect();
        ports.sort_unstable();
        ports.dedup();
        let name = match listed.names.first() {
            Some(name) => name.trim_start_matches('/').to_owned(),
            None => listed.id.chars().take(12).collect(),
        };
        Self {
            id: listed.id,
            name,
            labels: listed.labels.unwrap_or_default(),
            address,
            ports,
        }
    }
}

/// Parses the body of `GET /containers/json`.
fn parse_containers(body: &[u8]) -> Result<Vec<Container>, String> {
    let listed: Vec<Listed> =
        serde_json::from_slice(body).map_err(|err| format!("container list: {err}"))?;
    Ok(listed.into_iter().map(Container::from).collect())
}

/// What the labels of one container ask for.
struct Target {
    pool: String,
    policy: Option<Policy>,
    hosts: Vec<String>,
    tcp_listen: Option<SocketAddr>,
    backend: Backend,
}

impl Container {
    fn target(&self) -> Result<Target, String> {
        let mut pool: Option<&str> = None;
        let mut policy: Option<Policy> = None;
        let mut hosts: Vec<String> = Vec::new();
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
        let address = self
            .address
            .ok_or("has no IP address on a Docker network")?;

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
            tcp_listen,
            backend: Backend::new(SocketAddr::new(address, port).to_string()),
        })
    }

    /// `PROJECT-SERVICE` for Compose containers, so the replicas of a service
    /// share a pool, else the container name.
    fn default_pool(&self) -> String {
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
    pool: String,
    policy: Option<Policy>,
    /// Hosts of the HTTP server; unused with `tcp_listen`.
    hosts: Vec<String>,
    /// Address of the TCP server, or `None` for an HTTP route.
    tcp_listen: Option<SocketAddr>,
}

impl Route {
    fn add_to(&self, config: &mut Config, http_listen: SocketAddr) -> Result<(), String> {
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

        let name = format!("docker-{pool}");
        let forward = format!("@{pool}");
        let server = match self.tcp_listen {
            Some(listen) => {
                json!({ "name": name, "listen": [listen], "protocol": "tcp", "forward": forward })
            }
            None => {
                json!({ "name": name, "listen": [http_listen], "host": self.hosts, "forward": forward })
            }
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
            eprintln!("Docker => ignored {reason}");
        }
    }
}

/// Plans the routes to `containers` on top of `config`, which is valid, and
/// returns `config` with the pools and servers of those routes. A container
/// or pool that cannot be routed, or that would make the configuration
/// invalid, is left out. Without `[docker]` nothing is added.
pub(crate) fn plan(config: &Config, containers: &[Container]) -> (Config, Plan) {
    let mut effective = config.clone();
    let mut plan = Plan::default();
    let Some(settings) = &config.docker else {
        return (effective, plan);
    };

    let mut sorted: Vec<&Container> = containers.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    for container in sorted {
        let name = &container.name;
        let target = match container.target() {
            Ok(target) => target,
            Err(reason) => {
                plan.ignored.push(format!("container {name}: {reason}"));
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
                pool: target.pool,
                policy: target.policy,
                hosts: target.hosts,
                tcp_listen: target.tcp_listen,
            });
            continue;
        };
        if route.policy != target.policy || route.tcp_listen != target.tcp_listen {
            plan.ignored.push(format!(
                "container {name}: rxh.policy or rxh.tcp.listen differ from another container \
                 of pool {}",
                route.pool
            ));
            continue;
        }
        for host in target.hosts {
            if !route.hosts.contains(&host) {
                route.hosts.push(host);
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
            .add_to(&mut candidate, settings.listen)
            .and_then(|()| candidate.validate().map_err(|err| err.to_string()));
        match outcome {
            Ok(()) => {
                effective = candidate;
                true
            }
            Err(reason) => {
                ignored.push(format!("pool {}: {reason}", route.pool));
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

/// Sends the routable containers of the Docker daemon at `endpoint` to
/// `updates`: once after connecting, then after every container that starts
/// or dies. While Docker cannot be reached nothing is sent, so the last
/// containers stay routed, and the connection is retried forever.
pub(crate) async fn watch(endpoint: String, updates: mpsc::Sender<Vec<Container>>) {
    let mut last_error = String::new();
    loop {
        match follow(&endpoint, &updates).await {
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
async fn follow(endpoint: &str, updates: &mpsc::Sender<Vec<Container>>) -> Result<(), String> {
    // Subscribed before listing, so a container that starts in between is
    // listed again instead of missed.
    let mut events = get(endpoint, EVENTS).await?.into_body();
    let mut buffered: Vec<u8> = Vec::new();
    // Docker can announce a death while it still lists the container as
    // running: those are left out until they start again.
    let mut dead: BTreeSet<String> = BTreeSet::new();
    loop {
        let list = get(endpoint, CONTAINERS);
        let body = tokio::time::timeout(LIST_TIMEOUT, async {
            Limited::new(list.await?.into_body(), MAX_LIST_BODY)
                .collect()
                .await
                .map_err(|err| format!("container list: {err}"))
        })
        .await
        .map_err(|_| String::from("container list: timeout"))??;
        let mut containers = parse_containers(&body.to_bytes())?;
        containers.retain(|container| !dead.contains(&container.id));
        if updates.send(containers).await.is_err() {
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

async fn get(endpoint: &str, path: &str) -> Result<Response<Incoming>, String> {
    let mut sender = connect(endpoint)
        .await
        .map_err(|err| format!("connect: {err}"))?;
    let request = Request::get(path)
        .header(header::HOST, "docker")
        .body(Empty::<Bytes>::new())
        .expect("static request parts are valid");
    let response = sender
        .send_request(request)
        .await
        .map_err(|err| err.to_string())?;
    if !response.status().is_success() {
        return Err(format!("Docker API answered {}", response.status()));
    }
    Ok(response)
}

async fn connect(endpoint: &str) -> io::Result<SendRequest<Empty<Bytes>>> {
    match endpoint.split_once("://") {
        #[cfg(unix)]
        Some(("unix", path)) => handshake(tokio::net::UnixStream::connect(path).await?).await,
        Some(("tcp", address)) => handshake(TcpStream::connect(address).await?).await,
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported Docker endpoint",
        )),
    }
}

async fn handshake<S>(stream: S) -> io::Result<SendRequest<Empty<Bytes>>>
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
            id: format!("id-{name}"),
            name: name.to_owned(),
            labels,
            address: address.parse().ok(),
            ports: ports.to_vec(),
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
        let containers = parse_containers(body).unwrap();
        assert_eq!(containers[0].name, "shop-web-1");
        assert_eq!(containers[0].address, "172.20.0.5".parse().ok());
        assert_eq!(containers[0].ports, [80]);
        assert_eq!(containers[0].labels["rxh.http.host"], "app.test");
        assert_eq!(containers[1].name, "fedcba987654");
        assert_eq!(containers[1].address, "127.0.0.1".parse().ok());
        assert_eq!(containers[2].address, None);

        assert!(parse_containers(b"{\"message\":\"no\"}").is_err());
    }
}
