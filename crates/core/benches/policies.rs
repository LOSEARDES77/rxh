//! Criterion micro benchmarks of `Pool::select` for every load balancing
//! policy, with a small and a large pool.
//!
//! Run with `cargo bench -p rxh-core --bench policies`. `cargo test` runs
//! each benchmark once, which also checks that every policy picks a backend.

use std::hint;
use std::net::{IpAddr, Ipv4Addr};

use criterion::{BenchmarkId, Criterion};
use http::Uri;
use http::header::{self, HeaderMap, HeaderValue};
use rxh_core::config::{Backend, Policy, UpstreamGroup};
use rxh_core::pool::{Pool, SelectContext};

/// Pool sizes: a typical small group and a large one, where the cost of the
/// policies that look at every member (hashes, `least_conn`) shows.
const SIZES: [usize; 2] = [4, 64];

fn pool(policy: Policy, size: usize) -> Pool {
    let backends: Vec<Backend> = (0..size)
        .map(|index| Backend {
            address: format!("10.0.{}.{}:80", index / 256, index % 256),
            weight: u32::try_from(index % 3 + 1).expect("weights are 1 to 3"),
        })
        .collect();
    let mut group = UpstreamGroup::with_backends(backends);
    group.policy = policy;
    group.key = match policy {
        Policy::Query => Some("user".into()),
        Policy::Header => Some("x-user".into()),
        _ => None,
    };
    group.secret = Some("s3cret".into());
    Pool::new(policy.name(), &group).expect("generated backends are valid")
}

/// Request carrying every key a policy reads: IPs, path, query and header.
struct Request {
    ip: IpAddr,
    uri: Uri,
    headers: HeaderMap,
}

impl Request {
    fn new() -> Self {
        let mut headers = HeaderMap::new();
        headers.insert("x-user", HeaderValue::from_static("alice"));
        Self {
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)),
            uri: Uri::from_static("/api/items/42?user=alice&page=2"),
            headers,
        }
    }

    fn context(&self) -> SelectContext<'_> {
        SelectContext {
            peer_ip: self.ip,
            client_ip: self.ip,
            uri: Some(&self.uri),
            headers: Some(&self.headers),
            secure: false,
        }
    }

    /// Sends back the cookie a first selection set, so `cookie` measures the
    /// sticky path that every request after the first takes.
    fn stick(&mut self, pool: &Pool) {
        let selection = pool
            .select(&self.context())
            .expect("every backend is up, so a policy always picks one");
        let Some(set_cookie) = selection.set_cookie else {
            return;
        };
        let pair = set_cookie
            .split(';')
            .next()
            .expect("split yields at least one part");
        self.headers.insert(
            header::COOKIE,
            HeaderValue::from_str(pair).expect("cookie values are header safe"),
        );
        let again = pool
            .select(&self.context())
            .expect("the sticky backend is up");
        assert!(
            again.set_cookie.is_none() && again.upstream.address() == selection.upstream.address(),
            "a valid cookie must keep the request on its backend without a new cookie"
        );
    }
}

fn select(criterion: &mut Criterion) {
    for policy in Policy::ALL {
        let mut group = criterion.benchmark_group(format!("select/{}", policy.name()));
        for size in SIZES {
            let pool = pool(policy, size);
            let mut request = Request::new();
            request.stick(&pool);
            let context = request.context();
            group.bench_with_input(
                BenchmarkId::from_parameter(size),
                &context,
                |bencher, context| {
                    bencher.iter(|| pool.select(hint::black_box(context)));
                },
            );
        }
        group.finish();
    }
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    select(&mut criterion);
    criterion.final_summary();
}
