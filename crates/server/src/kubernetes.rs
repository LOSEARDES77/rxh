//! Kubernetes provider: the hosts of the Ingresses and of the Gateway API
//! HTTPRoutes are routed to the ready endpoints of their Services, which
//! become [`Container`]s planned like the instances of the service catalogs.
//!
//! Each Service port a host points at is a pool named
//! `NAMESPACE-SERVICE-PORT`, served on `[kubernetes] listen` for every host
//! that points at it, with the ready addresses of the EndpointSlices of the
//! Service as members.
//!
//! This is a first cut. Only whole hosts are routed: a path other than the
//! prefix `/`, a rule without a host, a default backend, HTTPRoute matches,
//! filters and traffic splitting are reported and left out. Gateways and
//! GatewayClasses are not read, so every HTTPRoute is served, whatever its
//! parents. Nothing is written back to the cluster (no status).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rxh_core::config::{Catalog, CatalogKind};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::catalog::Client;
use crate::docker::{Container, Provider};

const INGRESSES: &str = "/apis/networking.k8s.io/v1/ingresses";
const HTTP_ROUTES: &str = "/apis/gateway.networking.k8s.io/v1/httproutes";
const SERVICES: &str = "/api/v1/services";
const ENDPOINT_SLICES: &str = "/apis/discovery.k8s.io/v1/endpointslices";

/// Label of an EndpointSlice that names its Service.
const SERVICE_NAME: &str = "kubernetes.io/service-name";

/// Annotation that named the class of an Ingress before `ingressClassName`.
const INGRESS_CLASS: &str = "kubernetes.io/ingress.class";

/// Endpoints to route to, and the objects that cannot be routed, in every
/// namespace of the cluster.
pub(crate) async fn list(client: &Client, settings: &Catalog) -> Result<Vec<Container>, String> {
    let ingresses = client.get(INGRESSES, "Ingress list").await?;
    let routes = match client.get(HTTP_ROUTES, "HTTPRoute list").await {
        Ok(routes) => Some(routes),
        // A cluster without the Gateway API does not know the resource.
        Err(reason) if reason.ends_with("404 Not Found") => None,
        Err(reason) => return Err(reason),
    };
    let services = client.get(SERVICES, "Service list").await?;
    let slices = client.get(ENDPOINT_SLICES, "EndpointSlice list").await?;
    containers(
        &ingresses,
        routes.as_deref(),
        &services,
        &slices,
        settings.ingress_class.as_deref(),
    )
}

#[derive(Deserialize)]
struct List<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
struct Metadata {
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    annotations: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Ingress {
    metadata: Metadata,
    #[serde(default)]
    spec: IngressSpec,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct IngressSpec {
    ingress_class_name: Option<String>,
    default_backend: Option<Value>,
    #[serde(default)]
    rules: Vec<IngressRule>,
}

#[derive(Deserialize)]
struct IngressRule {
    host: Option<String>,
    http: Option<IngressHttp>,
}

#[derive(Deserialize)]
struct IngressHttp {
    paths: Vec<IngressPath>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IngressPath {
    path: Option<String>,
    path_type: Option<String>,
    backend: IngressBackend,
}

#[derive(Deserialize)]
struct IngressBackend {
    service: Option<IngressService>,
}

#[derive(Deserialize)]
struct IngressService {
    name: String,
    #[serde(default)]
    port: IngressPort,
}

#[derive(Deserialize, Default)]
struct IngressPort {
    number: Option<u16>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct HttpRoute {
    metadata: Metadata,
    #[serde(default)]
    spec: RouteSpec,
}

#[derive(Deserialize, Default)]
struct RouteSpec {
    #[serde(default)]
    hostnames: Vec<String>,
    #[serde(default)]
    rules: Vec<RouteRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteRule {
    #[serde(default)]
    matches: Vec<RouteMatch>,
    #[serde(default)]
    filters: Vec<Value>,
    #[serde(default)]
    backend_refs: Vec<BackendRef>,
}

#[derive(Deserialize)]
struct RouteMatch {
    path: Option<RoutePath>,
    /// `headers`, `queryParams` and `method`.
    #[serde(flatten)]
    others: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct RoutePath {
    #[serde(rename = "type")]
    kind: Option<String>,
    value: Option<String>,
}

impl RouteMatch {
    /// Whether every request matches.
    fn is_any(&self) -> bool {
        let any_path = self.path.as_ref().is_none_or(|path| {
            matches!(path.kind.as_deref(), None | Some("PathPrefix"))
                && matches!(path.value.as_deref(), None | Some("/"))
        });
        any_path && self.others.is_empty()
    }
}

#[derive(Deserialize)]
struct BackendRef {
    group: Option<String>,
    kind: Option<String>,
    name: String,
    namespace: Option<String>,
    port: Option<u16>,
}

#[derive(Deserialize)]
struct Service {
    metadata: Metadata,
    #[serde(default)]
    spec: ServiceSpec,
}

#[derive(Deserialize, Default)]
struct ServiceSpec {
    #[serde(default)]
    ports: Vec<ServicePort>,
}

#[derive(Deserialize)]
struct ServicePort {
    name: Option<String>,
    port: u16,
}

#[derive(Deserialize)]
struct EndpointSlice {
    metadata: Metadata,
    endpoints: Option<Vec<Endpoint>>,
    ports: Option<Vec<SlicePort>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Endpoint {
    addresses: Vec<String>,
    conditions: Option<Conditions>,
    target_ref: Option<TargetRef>,
}

#[derive(Deserialize)]
struct Conditions {
    ready: Option<bool>,
}

#[derive(Deserialize)]
struct TargetRef {
    name: Option<String>,
}

#[derive(Deserialize)]
struct SlicePort {
    name: Option<String>,
    port: Option<u16>,
    protocol: Option<String>,
}

/// A port of a Service, as an Ingress or an HTTPRoute names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PortRef {
    Number(u16),
    Name(String),
}

impl fmt::Display for PortRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => number.fmt(f),
            Self::Name(name) => name.fmt(f),
        }
    }
}

fn parse<T: DeserializeOwned>(body: &[u8], what: &str) -> Result<Vec<T>, String> {
    let list: List<T> = serde_json::from_slice(body).map_err(|err| format!("{what}: {err}"))?;
    Ok(list.items)
}

/// Something that cannot be routed, for the log: `name` is ignored for
/// `reason`.
fn rejected(name: &str, reason: String) -> Container {
    Container {
        rejected: Some(reason),
        ..endpoint(name.to_owned(), BTreeMap::new(), "", Vec::new())
    }
}

fn endpoint(
    name: String,
    labels: BTreeMap<String, String>,
    address: &str,
    ports: Vec<u16>,
) -> Container {
    Container {
        source: Provider::Catalog(CatalogKind::Kubernetes),
        id: name.clone(),
        name,
        labels,
        address: address.parse().ok(),
        ports,
        published: BTreeMap::new(),
        service: None,
        rejected: None,
    }
}

/// The endpoints to route to, from the bodies of the lists of the API;
/// `routes` is `None` without the Gateway API. With `class`, only the
/// Ingresses of that class count.
fn containers(
    ingresses: &[u8],
    routes: Option<&[u8]>,
    services: &[u8],
    slices: &[u8],
    class: Option<&str>,
) -> Result<Vec<Container>, String> {
    let ingresses: Vec<Ingress> = parse(ingresses, "Ingress list")?;
    let routes: Vec<HttpRoute> = match routes {
        Some(routes) => parse(routes, "HTTPRoute list")?,
        None => Vec::new(),
    };
    let services: Vec<Service> = parse(services, "Service list")?;
    let slices: Vec<EndpointSlice> = parse(slices, "EndpointSlice list")?;

    let mut containers: Vec<Container> = Vec::new();
    // Hosts of each Service port, by namespace, Service and port.
    let mut pools: BTreeMap<(String, String, PortRef), BTreeSet<String>> = BTreeMap::new();

    for ingress in ingresses {
        let Metadata {
            name,
            namespace,
            annotations,
            ..
        } = ingress.metadata;
        let spec = ingress.spec;
        let own_class = spec
            .ingress_class_name
            .as_deref()
            .or_else(|| annotations.get(INGRESS_CLASS).map(String::as_str));
        if class.is_some() && own_class != class {
            continue;
        }
        let name = format!("Ingress {namespace}/{name}");
        if spec.default_backend.is_some() {
            let reason = String::from("defaultBackend is not supported");
            containers.push(rejected(&name, reason));
        }
        for rule in spec.rules {
            let host = match rule.host {
                Some(host) if !host.is_empty() => host,
                _ => {
                    let reason = String::from("a rule without a host is not supported");
                    containers.push(rejected(&name, reason));
                    continue;
                }
            };
            for path in rule.http.map(|http| http.paths).unwrap_or_default() {
                let whole_host = matches!(path.path.as_deref(), None | Some("/"))
                    && path.path_type.as_deref() != Some("Exact");
                if !whole_host {
                    let reason = format!(
                        "host {host}: path {} is not supported, only the prefix /",
                        path.path.as_deref().unwrap_or("/")
                    );
                    containers.push(rejected(&name, reason));
                    continue;
                }
                let Some(service) = path.backend.service else {
                    let reason = format!("host {host}: resource backends are not supported");
                    containers.push(rejected(&name, reason));
                    continue;
                };
                let port = match (service.port.number, service.port.name) {
                    (Some(number), _) => PortRef::Number(number),
                    (None, Some(name)) => PortRef::Name(name),
                    (None, None) => {
                        let reason = format!("host {host}: Service {} has no port", service.name);
                        containers.push(rejected(&name, reason));
                        continue;
                    }
                };
                pools
                    .entry((namespace.clone(), service.name, port))
                    .or_default()
                    .insert(host.clone());
            }
        }
    }

    for route in routes {
        let Metadata {
            name, namespace, ..
        } = route.metadata;
        let name = format!("HTTPRoute {namespace}/{name}");
        let spec = route.spec;
        if spec.hostnames.is_empty() {
            let reason = "needs hostnames: those of the Gateway listeners are not read";
            containers.push(rejected(&name, reason.to_owned()));
            continue;
        }
        for rule in spec.rules {
            let backend = match rule.backend_refs.as_slice() {
                _ if !rule.matches.is_empty() && !rule.matches.iter().any(RouteMatch::is_any) => {
                    Err("a rule whose matches are not the path prefix / is not supported")
                }
                _ if !rule.filters.is_empty() => Err("filters are not supported"),
                [backend] => Ok(backend),
                _ => Err("a rule needs one backendRef, traffic is not split"),
            };
            let target = backend.and_then(|backend| {
                let is_service = backend.group.as_deref().unwrap_or_default().is_empty()
                    && matches!(backend.kind.as_deref(), None | Some("Service"));
                match backend.port {
                    Some(port) if is_service => Ok((backend, port)),
                    _ => Err("a backendRef must be a Service with a port"),
                }
            });
            match target {
                Ok((backend, port)) => {
                    let namespace = backend.namespace.as_ref().unwrap_or(&namespace);
                    pools
                        .entry((
                            namespace.clone(),
                            backend.name.clone(),
                            PortRef::Number(port),
                        ))
                        .or_default()
                        .extend(spec.hostnames.iter().cloned());
                }
                Err(reason) => containers.push(rejected(&name, reason.to_owned())),
            }
        }
    }

    for ((namespace, service, port), hosts) in pools {
        let name = format!("Service {namespace}/{service}");
        let found = services.iter().find(|candidate| {
            candidate.metadata.namespace == namespace && candidate.metadata.name == service
        });
        let Some(found) = found else {
            containers.push(rejected(&name, String::from("does not exist")));
            continue;
        };
        // An EndpointSlice names its ports like the Service does.
        let port_name = found.spec.ports.iter().find(|candidate| match &port {
            PortRef::Number(number) => candidate.port == *number,
            PortRef::Name(name) => candidate.name.as_ref() == Some(name),
        });
        let Some(port_name) = port_name.map(|port| port.name.as_deref().unwrap_or_default()) else {
            containers.push(rejected(&name, format!("has no port {port}")));
            continue;
        };

        let hosts: Vec<String> = hosts.into_iter().collect();
        let labels: BTreeMap<String, String> = BTreeMap::from([
            (String::from("rxh.enable"), String::from("true")),
            (String::from("rxh.http.host"), hosts.join(",")),
            (
                String::from("rxh.pool"),
                format!("{namespace}-{service}-{port}"),
            ),
        ]);
        for slice in &slices {
            let metadata = &slice.metadata;
            if metadata.namespace != namespace
                || metadata.labels.get(SERVICE_NAME) != Some(&service)
            {
                continue;
            }
            let target = slice.ports.iter().flatten().find(|candidate| {
                candidate.name.as_deref().unwrap_or_default() == port_name
                    && matches!(candidate.protocol.as_deref(), None | Some("TCP"))
            });
            let Some(target) = target.and_then(|target| target.port) else {
                continue;
            };
            for candidate in slice.endpoints.iter().flatten() {
                // Not set means ready.
                let ready = candidate.conditions.as_ref().and_then(|c| c.ready) != Some(false);
                let Some(address) = candidate.addresses.first().filter(|_| ready) else {
                    continue;
                };
                let pod = candidate
                    .target_ref
                    .as_ref()
                    .and_then(|target| target.name.as_ref())
                    .unwrap_or(address);
                containers.push(endpoint(
                    format!("{namespace}/{pod}:{target}"),
                    labels.clone(),
                    address,
                    vec![target],
                ));
            }
        }
    }
    Ok(containers)
}

#[cfg(test)]
mod tests {
    use rxh_core::config::Config;
    use serde_json::json;

    use super::*;
    use crate::docker;

    fn items(items: &[Value]) -> Vec<u8> {
        json!({ "kind": "List", "apiVersion": "v1", "items": items })
            .to_string()
            .into_bytes()
    }

    fn ingress(name: &str, class: Option<&str>, rules: Value) -> Value {
        json!({
            "metadata": { "name": name, "namespace": "default" },
            "spec": { "ingressClassName": class, "rules": rules },
        })
    }

    fn rule(host: &str, path: &str, service: &str, port: Value) -> Value {
        json!({ "host": host, "http": { "paths": [{
            "path": path, "pathType": "Prefix",
            "backend": { "service": { "name": service, "port": port } },
        }] } })
    }

    fn service(name: &str, ports: Value) -> Value {
        json!({
            "metadata": { "name": name, "namespace": "default" },
            "spec": { "type": "ClusterIP", "ports": ports },
        })
    }

    fn slice(service: &str, ports: Value, endpoints: Value) -> Value {
        json!({
            "metadata": {
                "name": format!("{service}-abcde"), "namespace": "default",
                "labels": { SERVICE_NAME: service },
            },
            "addressType": "IPv4", "ports": ports, "endpoints": endpoints,
        })
    }

    fn pod(name: &str, address: &str, ready: Value) -> Value {
        json!({
            "addresses": [address], "conditions": { "ready": ready },
            "targetRef": { "kind": "Pod", "name": name },
        })
    }

    /// The Service `web`, port 80 `http` and 9090 `metrics`, with two ready
    /// pods and one that is not.
    fn web() -> (Vec<u8>, Vec<u8>) {
        let ports = json!([
            { "name": "http", "port": 80, "targetPort": 8080 },
            { "name": "metrics", "port": 9090, "targetPort": 9100 },
        ]);
        let slice_ports = json!([
            { "name": "http", "port": 8080, "protocol": "TCP" },
            { "name": "metrics", "port": 9100, "protocol": "TCP" },
        ]);
        let endpoints = json!([
            pod("web-1", "10.1.0.1", json!(true)),
            pod("web-2", "10.1.0.2", json!(null)),
            pod("web-3", "10.1.0.3", json!(false)),
        ]);
        (
            items(&[service("web", ports)]),
            items(&[slice("web", slice_ports, endpoints)]),
        )
    }

    fn reasons(containers: &[Container]) -> Vec<String> {
        containers
            .iter()
            .filter_map(|container| {
                let reason = container.rejected.as_ref()?;
                Some(format!("{}: {reason}", container.name))
            })
            .collect()
    }

    #[test]
    fn ingress_hosts_are_routed_to_the_ready_endpoints() {
        let (services, slices) = web();
        let ingresses = items(&[
            ingress(
                "shop",
                None,
                json!([rule("shop.test", "/", "web", json!({ "number": 80 }))]),
            ),
            // The same Service port from another Ingress: one pool, two hosts.
            ingress(
                "www",
                None,
                json!([rule("www.shop.test", "/", "web", json!({ "number": 80 }))]),
            ),
            ingress(
                "metrics",
                None,
                json!([rule(
                    "metrics.test",
                    "/",
                    "web",
                    json!({ "name": "metrics" })
                )]),
            ),
        ]);
        let listed = containers(&ingresses, None, &services, &slices, None).unwrap();
        assert_eq!(reasons(&listed), [""; 0]);
        let summary: Vec<String> = listed
            .iter()
            .map(|c| format!("{} {:?} {:?}", c.name, c.address.unwrap(), c.ports))
            .collect();
        assert_eq!(
            summary,
            [
                "default/web-1:8080 10.1.0.1 [8080]",
                "default/web-2:8080 10.1.0.2 [8080]",
                "default/web-1:9100 10.1.0.1 [9100]",
                "default/web-2:9100 10.1.0.2 [9100]",
            ]
        );

        let config = Config::from_toml("[kubernetes]\nlisten = \"127.0.0.1:8080\"\n").unwrap();
        let (effective, plan) = docker::plan(&config, &listed);
        assert_eq!(plan.ignored, [""; 0]);
        let servers: Vec<String> = effective
            .servers
            .iter()
            .map(|server| format!("{} {:?}", server.name.as_deref().unwrap(), server.host))
            .collect();
        assert_eq!(
            servers,
            [
                "kubernetes-default-web-80 [\"shop.test\", \"www.shop.test\"]",
                "kubernetes-default-web-metrics [\"metrics.test\"]",
            ]
        );
        assert!(effective.upstreams.contains_key("default-web-80"));

        // Without `[kubernetes]` nothing is routed.
        let (effective, _) = docker::plan(&Config::default(), &listed);
        assert!(effective.servers.is_empty());
    }

    #[test]
    fn ingress_class_selects_the_ingresses() {
        let (services, slices) = web();
        let annotated = json!({
            "metadata": {
                "name": "old", "namespace": "default",
                "annotations": { INGRESS_CLASS: "rxh" },
            },
            "spec": { "rules": [rule("old.test", "/", "web", json!({ "number": 80 }))] },
        });
        let ingresses = items(&[
            ingress(
                "ours",
                Some("rxh"),
                json!([rule("ours.test", "/", "web", json!({ "number": 80 }))]),
            ),
            ingress(
                "theirs",
                Some("nginx"),
                json!([rule("theirs.test", "/", "web", json!({ "number": 80 }))]),
            ),
            ingress(
                "unset",
                None,
                json!([rule("unset.test", "/", "web", json!({ "number": 80 }))]),
            ),
            annotated,
        ]);
        let hosts = |class: Option<&str>| -> String {
            let listed = containers(&ingresses, None, &services, &slices, class).unwrap();
            listed[0].labels["rxh.http.host"].clone()
        };
        assert_eq!(hosts(Some("rxh")), "old.test,ours.test");
        assert_eq!(hosts(None), "old.test,ours.test,theirs.test,unset.test");
    }

    #[test]
    fn what_cannot_be_routed_is_reported() {
        let (services, slices) = web();
        let mut exact = rule("exact.test", "/", "web", json!({ "number": 80 }));
        exact["http"]["paths"][0]["pathType"] = json!("Exact");
        let mut resource = rule("files.test", "/", "web", json!({}));
        resource["http"]["paths"][0]["backend"] = json!({ "resource": { "kind": "Bucket" } });
        let mut fallback = ingress("fallback", None, json!([{ "http": { "paths": [] } }]));
        fallback["spec"]["defaultBackend"] = json!({ "service": { "name": "web" } });
        let ingresses = items(&[
            ingress(
                "mixed",
                None,
                json!([
                    rule("api.test", "/v1", "web", json!({ "number": 80 })),
                    exact,
                    resource,
                    rule("gone.test", "/", "gone", json!({ "number": 80 })),
                    rule("port.test", "/", "web", json!({ "number": 81 })),
                    rule("ok.test", "/", "web", json!({ "number": 80 })),
                ]),
            ),
            fallback,
        ]);
        let listed = containers(&ingresses, None, &services, &slices, None).unwrap();
        assert_eq!(
            reasons(&listed),
            [
                "Ingress default/mixed: host api.test: path /v1 is not supported, only the \
                 prefix /",
                "Ingress default/mixed: host exact.test: path / is not supported, only the \
                 prefix /",
                "Ingress default/mixed: host files.test: resource backends are not supported",
                "Ingress default/fallback: defaultBackend is not supported",
                "Ingress default/fallback: a rule without a host is not supported",
                "Service default/gone: does not exist",
                "Service default/web: has no port 81",
            ]
        );

        // Reported once by the plan, and the rest is routed.
        let config = Config::from_toml("[kubernetes]\n").unwrap();
        let (effective, plan) = docker::plan(&config, &listed);
        assert_eq!(effective.servers.len(), 1);
        assert_eq!(effective.servers[0].host, ["ok.test"]);
        assert_eq!(plan.ignored.len(), 7, "{:?}", plan.ignored);
        assert!(
            plan.ignored
                .iter()
                .any(|line| line
                    == "Kubernetes => ignored object Service default/gone: does not exist"),
            "{:?}",
            plan.ignored
        );
    }

    #[test]
    fn http_routes_are_routed_by_hostname() {
        let (services, slices) = web();
        let route = |name: &str, hostnames: Value, rules: Value| -> Value {
            json!({
                "metadata": { "name": name, "namespace": "default" },
                "spec": {
                    "parentRefs": [{ "name": "gateway" }],
                    "hostnames": hostnames, "rules": rules,
                },
            })
        };
        let web_ref = json!({ "name": "web", "port": 80 });
        let routes = items(&[
            route(
                "plain",
                json!(["a.test", "b.test"]),
                json!([{ "backendRefs": [web_ref] }]),
            ),
            route(
                "prefix",
                json!(["c.test"]),
                json!([{
                    "matches": [{ "path": { "type": "PathPrefix", "value": "/" } }],
                    "backendRefs": [{
                        "group": "", "kind": "Service", "name": "web",
                        "namespace": "default", "port": 80, "weight": 1,
                    }],
                }]),
            ),
            route("no-hosts", json!([]), json!([{ "backendRefs": [web_ref] }])),
            route(
                "bad",
                json!(["d.test"]),
                json!([
                    { "matches": [{ "path": { "type": "PathPrefix", "value": "/api" } }],
                      "backendRefs": [web_ref] },
                    { "matches": [{ "headers": [{ "name": "x", "value": "y" }] }],
                      "backendRefs": [web_ref] },
                    { "filters": [{ "type": "RequestRedirect" }], "backendRefs": [web_ref] },
                    { "backendRefs": [web_ref, { "name": "canary", "port": 80 }] },
                    { "backendRefs": [{ "kind": "Bucket", "name": "files", "port": 80 }] },
                    { "backendRefs": [{ "name": "web" }] },
                ]),
            ),
        ]);
        let listed = containers(&items(&[]), Some(&routes), &services, &slices, None).unwrap();
        assert_eq!(
            reasons(&listed),
            [
                "HTTPRoute default/no-hosts: needs hostnames: those of the Gateway listeners \
                 are not read",
                "HTTPRoute default/bad: a rule whose matches are not the path prefix / is not \
                 supported",
                "HTTPRoute default/bad: a rule whose matches are not the path prefix / is not \
                 supported",
                "HTTPRoute default/bad: filters are not supported",
                "HTTPRoute default/bad: a rule needs one backendRef, traffic is not split",
                "HTTPRoute default/bad: a backendRef must be a Service with a port",
                "HTTPRoute default/bad: a backendRef must be a Service with a port",
            ]
        );
        let routed: Vec<&Container> = listed.iter().filter(|c| c.rejected.is_none()).collect();
        assert_eq!(routed.len(), 2);
        assert_eq!(routed[0].labels["rxh.http.host"], "a.test,b.test,c.test");
        assert_eq!(routed[0].labels["rxh.pool"], "default-web-80");
    }

    #[test]
    fn lists_that_cannot_be_read_are_errors() {
        let (services, slices) = web();
        let empty = items(&[]);
        assert!(
            containers(&empty, None, &services, &slices, None)
                .unwrap()
                .is_empty()
        );
        let err = containers(b"<html>", None, &services, &slices, None).unwrap_err();
        assert!(err.starts_with("Ingress list: "), "{err}");
        let err = containers(&empty, Some(b"{}"), &services, &slices, None).unwrap_err();
        assert!(err.starts_with("HTTPRoute list: "), "{err}");
        let err = containers(&empty, None, &empty, b"[]", None).unwrap_err();
        assert!(err.starts_with("EndpointSlice list: "), "{err}");
    }
}
