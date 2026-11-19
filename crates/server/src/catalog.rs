//! Service catalog providers: the service instances of Consul, etcd and Nomad
//! become [`Container`]s, planned into pools and servers like the Docker
//! containers, with the same `rxh.*` options.
//!
//! - Consul: the services of the catalog tagged `rxh.enable=true`, and of
//!   those the instances whose health checks pass, with the options of
//!   their own `rxh.KEY=VALUE` tags. The default pool is the service name.
//! - Nomad: the services tagged `rxh.enable=true` in every namespace, with
//!   the options of the tags of each registration.
//! - etcd: the keys `PREFIX/SERVICE/INSTANCE/OPTION`, where `OPTION` is
//!   `address` (`ip:port`) or an option without its `rxh.` prefix
//!   (`http.host`, `pool`...). The default pool is `SERVICE`; an instance is
//!   enabled unless its `enable` key says otherwise.
//!
//! Catalogs are listed every `refresh`. While one cannot be reached nothing
//! is sent, so the instances last listed stay routed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request, header};
use rxh_core::address::BackendAddress;
use rxh_core::config::{Catalog, CatalogKind};
use serde::Deserialize;
use serde_json::json;

use crate::docker::{self, Container, Provider, Updates};

/// Sends the instances of the catalog `kind` to `updates`, every
/// `settings.refresh`, until nothing listens to `updates` anymore.
pub(crate) async fn watch(kind: CatalogKind, settings: Catalog, updates: Updates) {
    let source = Provider::Catalog(kind);
    let endpoint = settings.endpoint(kind);
    let mut last_error = String::new();
    loop {
        match list(kind, &settings).await {
            Ok(containers) => {
                last_error.clear();
                if updates.send((source, containers)).await.is_err() {
                    return;
                }
            }
            Err(reason) => {
                if reason != last_error {
                    eprintln!(
                        "{} => {endpoint}: {reason}, retrying every {:?}",
                        source.label(),
                        settings.refresh
                    );
                    last_error = reason;
                }
            }
        }
        tokio::time::sleep(settings.refresh).await;
    }
}

/// HTTP API of one catalog.
struct Client<'a> {
    /// `tcp://host:port`, as [`docker::fetch`] takes it.
    endpoint: String,
    authority: String,
    token: Option<(&'static str, &'a str)>,
}

impl<'a> Client<'a> {
    fn new(kind: CatalogKind, settings: &'a Catalog) -> Result<Self, String> {
        let endpoint = settings.endpoint(kind);
        let authority = BackendAddress::parse(endpoint)
            .ok()
            .and_then(|address| address.host_port())
            .ok_or_else(|| format!("invalid endpoint {endpoint:?}"))?;
        let token = match kind {
            CatalogKind::Consul => settings
                .token
                .as_deref()
                .map(|token| ("x-consul-token", token)),
            CatalogKind::Nomad => settings
                .token
                .as_deref()
                .map(|token| ("x-nomad-token", token)),
            CatalogKind::Etcd => None,
        };
        Ok(Self {
            endpoint: format!("tcp://{authority}"),
            authority,
            token,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Bytes,
        what: &str,
    ) -> Result<Bytes, String> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, &self.authority);
        if let Some((name, token)) = self.token {
            request = request.header(name, token);
        }
        let request = request
            .body(Full::new(body))
            .map_err(|err| format!("{what}: {err}"))?;
        docker::fetch(&self.endpoint, request, what).await
    }

    async fn get(&self, path: &str, what: &str) -> Result<Bytes, String> {
        self.send(Method::GET, path, Bytes::new(), what).await
    }
}

/// Instances of the catalog `kind` that may be routed.
async fn list(kind: CatalogKind, settings: &Catalog) -> Result<Vec<Container>, String> {
    let client = Client::new(kind, settings)?;
    let mut containers: Vec<Container> = Vec::new();
    match kind {
        CatalogKind::Consul => {
            let services = client.get("/v1/catalog/services", "service list").await?;
            for service in parse_consul_services(&services)? {
                let path = format!("/v1/health/service/{}?passing=1", encode(&service));
                let body = client.get(&path, "instance list").await?;
                containers.extend(parse_consul_instances(&body)?);
            }
        }
        CatalogKind::Nomad => {
            let services = client
                .get("/v1/services?namespace=*", "service list")
                .await?;
            for (namespace, service) in parse_nomad_services(&services)? {
                let path = format!(
                    "/v1/service/{}?namespace={}",
                    encode(&service),
                    encode(&namespace)
                );
                let body = client.get(&path, "instance list").await?;
                containers.extend(parse_nomad_instances(&body)?);
            }
        }
        CatalogKind::Etcd => {
            let prefix = settings.prefix();
            let body = client
                .send(Method::POST, "/v3/kv/range", range(prefix), "key range")
                .await?;
            containers = parse_etcd(&body, prefix)?;
        }
    }
    Ok(containers)
}

/// Percent-encodes `text` for a path segment or a query value.
fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'*') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

/// Options of the `rxh.KEY=VALUE` tags; other tags are not for rxh.
fn tag_labels(tags: &[String]) -> BTreeMap<String, String> {
    tags.iter()
        .filter(|tag| tag.starts_with("rxh."))
        .map(|tag| match tag.split_once('=') {
            Some((key, value)) => (key.to_owned(), value.to_owned()),
            None => (tag.clone(), String::new()),
        })
        .collect()
}

fn is_enabled(tags: &[String]) -> bool {
    tags.iter().any(|tag| tag == "rxh.enable=true")
}

/// The instance `name` of `service` at `address`, a bare IP.
fn instance(
    kind: CatalogKind,
    name: String,
    service: String,
    labels: BTreeMap<String, String>,
    address: &str,
    port: u16,
) -> Container {
    Container {
        source: Provider::Catalog(kind),
        id: name.clone(),
        name,
        labels,
        address: address.parse().ok(),
        ports: if port == 0 { Vec::new() } else { vec![port] },
        published: BTreeMap::new(),
        service: Some(service),
    }
}

/// Names of the services tagged `rxh.enable=true` in the body of Consul's
/// `GET /v1/catalog/services`.
fn parse_consul_services(body: &[u8]) -> Result<Vec<String>, String> {
    let services: BTreeMap<String, Option<Vec<String>>> =
        serde_json::from_slice(body).map_err(|err| format!("service list: {err}"))?;
    Ok(services
        .into_iter()
        .filter(|(_, tags)| is_enabled(tags.as_deref().unwrap_or_default()))
        .map(|(name, _)| name)
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConsulEntry {
    node: ConsulNode,
    service: ConsulService,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConsulNode {
    node: String,
    #[serde(default)]
    address: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConsulService {
    #[serde(rename = "ID")]
    id: String,
    service: String,
    tags: Option<Vec<String>>,
    #[serde(default)]
    address: String,
    port: u16,
}

/// Body of Consul's `GET /v1/health/service/NAME?passing=1`.
fn parse_consul_instances(body: &[u8]) -> Result<Vec<Container>, String> {
    let entries: Vec<ConsulEntry> =
        serde_json::from_slice(body).map_err(|err| format!("instance list: {err}"))?;
    Ok(entries
        .into_iter()
        .map(|entry| {
            let service = entry.service;
            // An empty service address means the address of the node.
            let address = if service.address.is_empty() {
                &entry.node.address
            } else {
                &service.address
            };
            instance(
                CatalogKind::Consul,
                format!("{}/{}", entry.node.node, service.id),
                service.service,
                tag_labels(service.tags.as_deref().unwrap_or_default()),
                address,
                service.port,
            )
        })
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NomadNamespace {
    namespace: String,
    services: Option<Vec<NomadStub>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NomadStub {
    service_name: String,
    tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NomadRegistration {
    service_name: String,
    #[serde(rename = "AllocID", default)]
    alloc_id: String,
    tags: Option<Vec<String>>,
    #[serde(default)]
    address: String,
    port: u16,
}

/// Namespaces and names of the services tagged `rxh.enable=true` in the
/// body of Nomad's `GET /v1/services?namespace=*`.
fn parse_nomad_services(body: &[u8]) -> Result<Vec<(String, String)>, String> {
    let namespaces: Vec<NomadNamespace> =
        serde_json::from_slice(body).map_err(|err| format!("service list: {err}"))?;
    let mut services: Vec<(String, String)> = Vec::new();
    for namespace in namespaces {
        for stub in namespace.services.unwrap_or_default() {
            if is_enabled(stub.tags.as_deref().unwrap_or_default()) {
                services.push((namespace.namespace.clone(), stub.service_name));
            }
        }
    }
    Ok(services)
}

/// Body of Nomad's `GET /v1/service/NAME`.
fn parse_nomad_instances(body: &[u8]) -> Result<Vec<Container>, String> {
    let registrations: Vec<NomadRegistration> =
        serde_json::from_slice(body).map_err(|err| format!("instance list: {err}"))?;
    Ok(registrations
        .into_iter()
        .map(|registration| {
            let allocation: String = registration.alloc_id.chars().take(8).collect();
            instance(
                CatalogKind::Nomad,
                format!("{}.{allocation}", registration.service_name),
                registration.service_name,
                tag_labels(registration.tags.as_deref().unwrap_or_default()),
                &registration.address,
                registration.port,
            )
        })
        .collect())
}

/// Body of an etcd `POST /v3/kv/range` for every key under `prefix`, which
/// is not empty.
fn range(prefix: &str) -> Bytes {
    use base64::Engine;

    let mut end: Vec<u8> = prefix.as_bytes().to_vec();
    // The range ends at the first key past the prefix: its last byte that
    // can be incremented, incremented, and what follows dropped.
    while end.last() == Some(&0xff) {
        end.pop();
    }
    match end.last_mut() {
        Some(last) => *last += 1,
        // Every byte is 0xff: up to the end of the key space.
        None => end.push(0),
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let body = json!({ "key": engine.encode(prefix), "range_end": engine.encode(end) });
    Bytes::from(body.to_string())
}

#[derive(Deserialize)]
struct EtcdRange {
    #[serde(default)]
    kvs: Vec<EtcdPair>,
}

#[derive(Deserialize)]
struct EtcdPair {
    key: String,
    #[serde(default)]
    value: String,
}

/// Body of etcd's `POST /v3/kv/range` over `prefix`: one instance per
/// `SERVICE/INSTANCE` under it.
fn parse_etcd(body: &[u8], prefix: &str) -> Result<Vec<Container>, String> {
    use base64::Engine;

    let engine = base64::engine::general_purpose::STANDARD;
    let decode = |text: &str| -> Result<String, String> {
        let bytes = engine
            .decode(text)
            .map_err(|err| format!("key range: {err}"))?;
        String::from_utf8(bytes).map_err(|err| format!("key range: {err}"))
    };
    let range: EtcdRange =
        serde_json::from_slice(body).map_err(|err| format!("key range: {err}"))?;
    let mut instances: BTreeMap<(String, String), BTreeMap<String, String>> = BTreeMap::new();
    for pair in range.kvs {
        let key = decode(&pair.key)?;
        let Some(path) = key.strip_prefix(prefix) else {
            continue;
        };
        // Keys of another layout under the prefix are not instances.
        let mut parts = path.splitn(3, '/');
        let (Some(service), Some(name), Some(option)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        instances
            .entry((service.to_owned(), name.to_owned()))
            .or_default()
            .insert(option.to_owned(), decode(&pair.value)?.trim().to_owned());
    }

    let mut containers: Vec<Container> = Vec::new();
    for ((service, name), options) in instances {
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        labels.insert(String::from("rxh.enable"), String::from("true"));
        let mut address: Option<IpAddr> = None;
        let mut port: u16 = 0;
        for (option, value) in options {
            if option != "address" {
                labels.insert(format!("rxh.{option}"), value);
            } else if let Ok(socket) = value.parse::<SocketAddr>() {
                (address, port) = (Some(socket.ip()), socket.port());
            } else {
                address = value.parse().ok();
            }
        }
        let mut container = instance(
            CatalogKind::Etcd,
            format!("{service}/{name}"),
            service,
            labels,
            "",
            port,
        );
        container.address = address;
        containers.push(container);
    }
    Ok(containers)
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Config;

    use super::*;

    fn members(config: &Config, containers: &[Container]) -> (Vec<String>, Vec<String>) {
        let (effective, plan) = docker::plan(config, containers);
        let servers: Vec<String> = effective
            .servers
            .iter()
            .map(|server| format!("{} {:?}", server.name.as_deref().unwrap_or(""), server.host))
            .collect();
        (servers, plan.ignored)
    }

    #[test]
    fn consul_instances_with_rxh_tags_are_routed() {
        let services = br#"{"consul":[],"web":["rxh.enable=true","v1"],"db":null,
                            "off":["rxh.enable=false"]}"#;
        assert_eq!(parse_consul_services(services).unwrap(), ["web"]);
        assert!(parse_consul_services(b"[]").is_err());

        let body = br#"[
          {"Node":{"Node":"n1","Address":"10.0.0.1"},
           "Service":{"ID":"web-1","Service":"web","Tags":["rxh.enable=true",
             "rxh.http.host=app.test","v1"],"Address":"10.0.1.1","Port":8080},
           "Checks":[{"Status":"passing"}]},
          {"Node":{"Node":"n2","Address":"10.0.0.2"},
           "Service":{"ID":"web-2","Service":"web","Tags":["rxh.enable=true",
             "rxh.http.host=app.test"],"Address":"","Port":8081}}
        ]"#;
        let instances = parse_consul_instances(body).unwrap();
        assert_eq!(instances[0].name, "n1/web-1");
        assert_eq!(instances[0].address, "10.0.1.1".parse().ok());
        assert_eq!(instances[0].ports, [8080]);
        assert!(!instances[0].labels.contains_key("v1"));
        // Without a service address, the node's.
        assert_eq!(instances[1].address, "10.0.0.2".parse().ok());

        let config = Config::from_toml("[consul]\nlisten = \"127.0.0.1:8080\"\n").unwrap();
        let (effective, plan) = docker::plan(&config, &instances);
        assert_eq!(plan.ignored, [""; 0]);
        assert_eq!(effective.servers[0].name.as_deref(), Some("consul-web"));
        assert_eq!(
            effective.servers[0].listen,
            ["127.0.0.1:8080".parse().unwrap()]
        );
        assert!(effective.upstreams.contains_key("web"));

        // Without `[consul]` the instances are not routed.
        assert!(
            docker::plan(&Config::default(), &instances)
                .0
                .servers
                .is_empty()
        );
    }

    #[test]
    fn nomad_services_of_every_namespace_are_routed() {
        let services = br#"[
          {"Namespace":"default","Services":[
            {"ServiceName":"api","Tags":["rxh.enable=true"]},
            {"ServiceName":"cache","Tags":[]}]},
          {"Namespace":"prod","Services":[{"ServiceName":"shop","Tags":["rxh.enable=true"]}]},
          {"Namespace":"empty","Services":null}
        ]"#;
        assert_eq!(
            parse_nomad_services(services).unwrap(),
            [
                ("default".to_owned(), "api".to_owned()),
                ("prod".to_owned(), "shop".to_owned())
            ]
        );

        let body = br#"[
          {"ID":"_nomad-task-1","ServiceName":"api","Namespace":"default","NodeID":"n",
           "Datacenter":"dc1","JobID":"api","AllocID":"0123456789abcdef",
           "Tags":["rxh.enable=true","rxh.tcp.listen=:7000"],"Address":"10.0.2.1","Port":25000},
          {"ID":"_nomad-task-2","ServiceName":"api","AllocID":"fedcba98",
           "Tags":["rxh.enable=true","rxh.tcp.listen=:7000"],"Address":"10.0.2.2","Port":25001}
        ]"#;
        let instances = parse_nomad_instances(body).unwrap();
        assert_eq!(instances[0].name, "api.01234567");
        let config = Config::from_toml("[nomad]\n").unwrap();
        let (servers, ignored) = members(&config, &instances);
        assert_eq!(ignored, [""; 0]);
        assert_eq!(servers, ["nomad-api []"]);
        let (_, plan) = docker::plan(&config, &instances);
        assert_eq!(plan.routes.len(), 1);
    }

    #[test]
    fn etcd_keys_under_the_prefix_are_instances() {
        use base64::Engine;

        let engine = base64::engine::general_purpose::STANDARD;
        let pair = |key: &str, value: &str| {
            json!({ "key": engine.encode(key), "value": engine.encode(value),
                    "create_revision": "5", "version": "1" })
        };
        let body = json!({
            "header": { "revision": "9" },
            "kvs": [
                pair("rxh/web/a/address", "10.0.3.1:80"),
                pair("rxh/web/a/http.host", "app.test"),
                pair("rxh/web/b/address", " 10.0.3.2 "),
                pair("rxh/web/b/http.host", "app.test"),
                pair("rxh/web/b/http.port", "81"),
                pair("rxh/web/c/address", "10.0.3.3:80"),
                pair("rxh/web/c/http.host", "app.test"),
                pair("rxh/web/c/enable", "false"),
                pair("rxh/stray", "x"),
                pair("other/web/d/address", "10.0.3.4:80"),
            ],
            "count": "10",
        })
        .to_string();
        let instances = parse_etcd(body.as_bytes(), "rxh/").unwrap();
        let names: Vec<&str> = instances.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["web/a", "web/b", "web/c"]);
        assert_eq!(instances[1].address, "10.0.3.2".parse().ok());
        assert!(instances[1].ports.is_empty());

        let config = Config::from_toml("[etcd]\n").unwrap();
        let (effective, plan) = docker::plan(&config, &instances);
        assert_eq!(plan.routes.len(), 1);
        assert_eq!(effective.servers[0].name.as_deref(), Some("etcd-web"));
        assert_eq!(plan.ignored.len(), 1, "{:?}", plan.ignored);
        assert!(plan.ignored[0].contains("etcd => ignored instance web/c: rxh.enable is not true"));

        // An empty range has no `kvs`.
        assert!(parse_etcd(b"{\"header\":{}}", "rxh/").unwrap().is_empty());
        assert!(parse_etcd(b"{\"kvs\":[{\"key\":\"!\"}]}", "rxh/").is_err());
    }

    #[test]
    fn etcd_range_covers_the_prefix() {
        let body =
            |prefix: &str| -> serde_json::Value { serde_json::from_slice(&range(prefix)).unwrap() };
        // "rxh/" and "rxh0".
        assert_eq!(
            body("rxh/"),
            json!({ "key": "cnhoLw==", "range_end": "cnhoMA==" })
        );
        assert_eq!(body("\u{7f}")["range_end"], "gA==");
    }

    #[test]
    fn a_pool_belongs_to_one_provider() {
        let config =
            Config::from_toml("[docker]\nlisten = \"127.0.0.1:8080\"\n[consul]\n[etcd]\n").unwrap();
        let labels = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect()
        };
        let web = labels(&[("rxh.enable", "true"), ("rxh.http.host", "app.test")]);
        let mut docker = instance(
            CatalogKind::Consul,
            "d".into(),
            "web".into(),
            web.clone(),
            "10.0.0.1",
            80,
        );
        docker.source = Provider::Docker;
        let consul = instance(
            CatalogKind::Consul,
            "c".into(),
            "web".into(),
            web.clone(),
            "10.0.0.2",
            80,
        );
        let etcd = instance(
            CatalogKind::Etcd,
            "e".into(),
            "api".into(),
            labels(&[("rxh.enable", "true"), ("rxh.http.host", "api.test")]),
            "10.0.0.3",
            80,
        );
        let (servers, ignored) = members(&config, &[etcd, consul, docker]);
        // Docker plans first; Consul's instance of the same pool is left out.
        assert_eq!(
            servers,
            ["docker-web [\"app.test\"]", "etcd-api [\"api.test\"]"]
        );
        assert_eq!(ignored.len(), 1, "{ignored:?}");
        assert!(
            ignored[0]
                .contains("Consul => ignored instance c: pool web is already routed by Docker"),
            "{ignored:?}"
        );
    }

    #[test]
    fn names_are_percent_encoded() {
        assert_eq!(encode("web-1.v2_x~*"), "web-1.v2_x~*");
        assert_eq!(encode("a b/c?d"), "a%20b%2Fc%3Fd");
    }
}
