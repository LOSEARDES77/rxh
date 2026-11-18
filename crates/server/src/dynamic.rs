//! Members discovered through DNS: periodic `A`/`AAAA` and `SRV` lookups that
//! feed [`Pool::sync_dynamic`](rxh_core::pool::Pool::sync_dynamic).

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use hickory_resolver::config::{LookupIpStrategy, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::rdata::SRV;
use hickory_resolver::{Resolver, TokioResolver};
use rxh_core::config::{Backend, DynamicUpstreams, IpVersion};
use tokio::time::MissedTickBehavior;

use crate::registry::Group;

/// Refreshes the DNS members of `group` forever. A failed lookup keeps the
/// previous members and is logged.
pub(crate) async fn run(group: Arc<Group>) {
    let Some(settings) = group.pool.settings().dynamic.clone() else {
        return;
    };
    let resolver = match build_resolver(&settings) {
        Ok(resolver) => resolver,
        Err(reason) => {
            eprintln!(
                "{} => dynamic upstreams disabled: {reason}",
                group.pool.name()
            );
            return;
        }
    };

    let mut interval = tokio::time::interval(settings.refresh);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match lookup(&resolver, &settings).await {
            Ok(backends) => group.pool.sync_dynamic(backends),
            Err(reason) => eprintln!("{} => dns refresh failed: {reason}", group.pool.name()),
        }
    }
}

fn build_resolver(settings: &DynamicUpstreams) -> Result<TokioResolver, String> {
    let mut builder = if settings.resolvers.is_empty() {
        Resolver::builder_tokio().map_err(|err| err.to_string())?
    } else {
        let servers = settings
            .resolvers
            .iter()
            .map(|resolver| {
                let mut server =
                    hickory_resolver::config::NameServerConfig::udp_and_tcp(resolver.ip());
                for connection in &mut server.connections {
                    connection.port = resolver.port();
                }
                server
            })
            .collect();
        Resolver::builder_with_config(
            ResolverConfig::from_name_servers(servers),
            TokioRuntimeProvider::default(),
        )
    };
    // The refresh interval is the only cache.
    builder.options_mut().cache_size = 0;
    builder.options_mut().ip_strategy = match settings.versions.as_deref().unwrap_or_default() {
        [IpVersion::Ipv4] => LookupIpStrategy::Ipv4Only,
        [IpVersion::Ipv6] => LookupIpStrategy::Ipv6Only,
        _ => LookupIpStrategy::Ipv4AndIpv6,
    };
    builder.build().map_err(|err| err.to_string())
}

async fn lookup(
    resolver: &TokioResolver,
    settings: &DynamicUpstreams,
) -> Result<Vec<Backend>, String> {
    if let Some(name) = &settings.srv {
        let lookup = resolver
            .srv_lookup(name.as_str())
            .await
            .map_err(|err| format!("srv {name}: {err}"))?;
        let records: Vec<&SRV> = lookup
            .answers()
            .iter()
            .filter_map(|record| match &record.data {
                RData::SRV(srv) => Some(srv),
                _ => None,
            })
            .collect();
        return Ok(srv_backends(records));
    }

    let name = settings.a.as_deref().unwrap_or_default();
    let (host, port) = name
        .rsplit_once(':')
        .and_then(|(host, port)| Some((host, port.parse::<u16>().ok()?)))
        .ok_or_else(|| format!("invalid dynamic.a {name:?}"))?;
    let lookup = resolver
        .lookup_ip(host)
        .await
        .map_err(|err| format!("a {name}: {err}"))?;
    Ok(ip_backends(lookup.iter(), port))
}

/// Backends for resolved IPs, sorted and without duplicates.
pub(crate) fn ip_backends(ips: impl IntoIterator<Item = IpAddr>, port: u16) -> Vec<Backend> {
    let mut addresses: Vec<SocketAddr> = ips
        .into_iter()
        .map(|ip| SocketAddr::new(ip, port))
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
        .into_iter()
        .map(|address| Backend::new(address.to_string()))
        .collect()
}

/// Backends for `SRV` records: `target:port` with the target's trailing dot
/// removed and the record weight, ordered by priority.
pub(crate) fn srv_backends<'a>(records: impl IntoIterator<Item = &'a SRV>) -> Vec<Backend> {
    // A root target (".") means the service is explicitly unavailable.
    let mut records: Vec<&SRV> = records
        .into_iter()
        .filter(|srv| !srv.target.is_root())
        .collect();
    records.sort_by_key(|srv| (srv.priority, srv.target.to_ascii(), srv.port));
    let mut backends: Vec<Backend> = Vec::new();
    for srv in records {
        let target = srv.target.to_ascii();
        let address = format!("{}:{}", target.trim_end_matches('.'), srv.port);
        if backends.iter().any(|backend| backend.address == address) {
            continue;
        }
        backends.push(Backend {
            address,
            // Weight 0 means "rarely" in SRV but would disable the member here.
            weight: u32::from(srv.weight).max(1),
        });
    }
    backends
}

#[cfg(test)]
mod tests {
    use hickory_resolver::proto::rr::Name;

    use super::*;

    #[test]
    fn srv_records_become_backends() {
        let name = |text: &str| Name::from_ascii(text).unwrap();
        let records = [
            SRV::new(20, 5, 8081, name("b.example.com.")),
            SRV::new(10, 0, 8080, name("a.example.com.")),
            SRV::new(10, 0, 8080, name("a.example.com.")),
            SRV::new(1, 0, 80, Name::root()),
        ];
        let backends = srv_backends(&records);
        assert_eq!(
            backends,
            [
                Backend::new("a.example.com:8080"),
                Backend {
                    address: "b.example.com:8081".into(),
                    weight: 5
                },
            ]
        );
    }

    #[test]
    fn ips_become_sorted_unique_backends() {
        let ips = ["10.0.0.2", "::1", "10.0.0.1", "10.0.0.2"].map(|ip| ip.parse().unwrap());
        let addresses: Vec<String> = ip_backends(ips, 80)
            .into_iter()
            .map(|backend| backend.address)
            .collect();
        assert_eq!(addresses, ["10.0.0.1:80", "10.0.0.2:80", "[::1]:80"]);
    }
}
