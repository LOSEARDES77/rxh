//! Property tests of the load balancing policies: weighted round robin
//! honours the weights, hash policies only move the keys of a backend that
//! leaves, and no policy ever picks a backend that is down or full.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use http::Uri;
use http::header::{self, HeaderMap};
use proptest::prelude::*;
use proptest::sample;
use rxh_core::config::{Backend, Fallback, PassiveHealth, Policy, UpstreamGroup};
use rxh_core::pool::{Pool, RequestGuard, SelectContext, Upstream};

const POLICIES: [Policy; 12] = [
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

/// Policies that hash a request key with rendezvous hashing.
const HASH_POLICIES: [Policy; 5] = [
    Policy::IpHash,
    Policy::ClientIpHash,
    Policy::UriHash,
    Policy::Query,
    Policy::Header,
];

const SECRET: &str = "s3cret";

fn address(index: usize) -> String {
    format!("10.0.0.{index}:80")
}

/// Key setting each policy reads, the same for the policy and its fallback.
fn key_for(policy: Policy) -> Option<String> {
    match policy {
        Policy::Query => Some("user".into()),
        Policy::Header => Some("x-user".into()),
        _ => None,
    }
}

fn group(policy: Policy, weights: &[u32]) -> UpstreamGroup {
    let backends: Vec<Backend> = weights
        .iter()
        .enumerate()
        .map(|(index, weight)| Backend {
            address: address(index),
            weight: *weight,
        })
        .collect();
    let mut group = UpstreamGroup::with_backends(backends);
    group.policy = policy;
    group.key = key_for(policy);
    group.secret = Some(SECRET.into());
    group
}

/// Request whose every hashable part (IPs, path, query, header) derives from
/// `key`, so all hash policies see a key with the same identity.
struct Request {
    ip: IpAddr,
    uri: Uri,
    headers: HeaderMap,
}

impl Request {
    fn new(key: u32) -> Self {
        let uri: Uri = format!("/k{key}?user=k{key}")
            .parse()
            .expect("digits make a valid URI");
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-user",
            format!("k{key}")
                .parse()
                .expect("digits make a valid header"),
        );
        Self {
            ip: IpAddr::V4(Ipv4Addr::from(key)),
            uri,
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
}

fn owner(pool: &Pool, key: u32) -> Option<String> {
    let request = Request::new(key);
    pool.select(&request.context())
        .map(|selection| selection.upstream.address().to_owned())
}

fn owners(pool: &Pool, keys: &[u32]) -> Vec<Option<String>> {
    keys.iter().map(|key| owner(pool, *key)).collect()
}

fn mark_down(upstream: &Upstream) {
    assert_eq!(
        upstream.record_check(/* passed= */ false, 1, 1),
        Some(false),
        "a healthy backend goes down after one failed check with fails = 1"
    );
}

/// How a backend is set up before selecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Up,
    /// Marked down by an active health check.
    Down,
    /// Marked down by passive health checks (`max_fails` reached).
    Failed,
    /// At its `unhealthy_request_count` limit.
    Full,
}

fn state() -> impl Strategy<Value = State> {
    prop_oneof![
        Just(State::Up),
        Just(State::Down),
        Just(State::Failed),
        Just(State::Full)
    ]
}

/// `Cookie` header pair that pins `address` under the `lb` cookie.
fn cookie_for(address: &str) -> String {
    let mut single = UpstreamGroup::with_backends(vec![Backend::new(address)]);
    single.policy = Policy::Cookie;
    single.secret = Some(SECRET.into());
    let pool = Pool::new("cookie", &single).expect("one valid backend");
    let request = Request::new(0);
    let set_cookie = pool
        .select(&request.context())
        .and_then(|selection| selection.set_cookie)
        .expect("a request without the cookie gets one");
    set_cookie
        .split(';')
        .next()
        .expect("split yields at least one part")
        .to_owned()
}

proptest! {
    /// Over whole cycles of smooth weighted round robin, every backend is
    /// picked exactly as many times as its weight.
    #[test]
    fn weighted_round_robin_honours_weights(
        weights in prop::collection::vec(0_u32..=8, 1..=8),
        cycles in 1_u32..=3,
    ) {
        let total: u32 = weights.iter().sum();
        prop_assume!(total > 0);
        let pool = Pool::new("wrr", &group(Policy::WeightedRoundRobin, &weights))
            .expect("generated addresses are valid and distinct");

        let mut counts = vec![0_u32; weights.len()];
        let request = Request::new(0);
        for _ in 0..total * cycles {
            let picked = pool.select(&request.context()).expect("a healthy backend has weight");
            let index = pool
                .upstreams()
                .iter()
                .position(|upstream| Arc::ptr_eq(upstream, &picked.upstream))
                .expect("the pick is a member");
            counts[index] += 1;
        }
        let expected: Vec<u32> = weights.iter().map(|weight| weight * cycles).collect();
        prop_assert_eq!(counts, expected);
    }

    /// When a backend leaves (removed, or marked down), only the keys it owned
    /// move; when it comes back healthy, they return to it.
    #[test]
    fn hashing_only_moves_keys_of_a_departed_backend(
        policy in sample::select(&HASH_POLICIES[..]),
        size in 2_usize..=8,
        leaving in any::<sample::Index>(),
        keys in prop::collection::vec(any::<u32>(), 1..=64),
        remove in any::<bool>(),
    ) {
        let pool = Pool::new("hash", &group(policy, &vec![1; size]))
            .expect("generated addresses are valid and distinct");
        let leaving = address(leaving.index(size));
        let before = owners(&pool, &keys);

        if remove {
            prop_assert!(pool.remove(&leaving));
        } else {
            let upstream = pool
                .upstreams()
                .into_iter()
                .find(|upstream| upstream.address() == leaving)
                .expect("the leaving backend is a member");
            mark_down(&upstream);
        }
        let after = owners(&pool, &keys);

        for (old, new) in before.iter().zip(&after) {
            let old = old.as_deref().expect("every backend was healthy");
            let new = new.as_deref().expect("other backends are still healthy");
            if old == leaving {
                prop_assert_ne!(new, leaving.as_str());
            } else {
                prop_assert_eq!(old, new);
            }
        }

        // Coming back restores the original owners.
        if remove {
            pool.add(Backend::new(leaving.clone())).expect("the address left the pool");
        } else {
            let upstream = pool
                .upstreams()
                .into_iter()
                .find(|upstream| upstream.address() == leaving)
                .expect("a backend marked down stays a member");
            prop_assert_eq!(upstream.record_check(/* passed= */ true, 1, 1), Some(true));
        }
        prop_assert_eq!(owners(&pool, &keys), before);
    }

    /// Whatever the policy, its fallback and the request, a pick is always an
    /// available backend, and nothing is picked only when none is available.
    #[test]
    fn no_policy_picks_an_unavailable_backend(
        policy in sample::select(&POLICIES[..]),
        fallback in prop::option::of(sample::select(&POLICIES[..])),
        members in prop::collection::vec((state(), 0_u32..=3), 1..=8),
        key in any::<u32>(),
        with_parts in any::<bool>(),
        pinned in prop::option::of(any::<sample::Index>()),
    ) {
        let weights: Vec<u32> = members.iter().map(|(_, weight)| *weight).collect();
        let mut config = group(policy, &weights);
        config.fallback = fallback.map(|policy| Fallback {
            policy,
            choose: 2,
            key: key_for(policy),
            secret: Some(SECRET.into()),
            fallback: None,
        });
        config.passive = Some(PassiveHealth {
            fail_duration: Duration::from_secs(3600),
            max_fails: 1,
            unhealthy_status: Vec::new(),
            unhealthy_latency: None,
            unhealthy_request_count: 1,
        });
        let pool = Pool::new("health", &config)
            .expect("generated addresses are valid and distinct");

        let upstreams = pool.upstreams();
        let mut guards: Vec<RequestGuard> = Vec::new();
        for (upstream, (state, _)) in upstreams.iter().zip(&members) {
            match state {
                State::Up => {}
                State::Down => mark_down(upstream),
                State::Failed => upstream.record_failure(),
                State::Full => guards.push(upstream.start_request()),
            }
        }
        for (upstream, (state, _)) in upstreams.iter().zip(&members) {
            prop_assert_eq!(upstream.is_available(), *state == State::Up);
        }

        // Requests with or without the hashed parts, possibly pinned by a
        // cookie to any member, healthy or not.
        let mut request = Request::new(key);
        if !with_parts {
            request.uri = Uri::from_static("/");
            request.headers.clear();
        }
        if let Some(pinned) = pinned {
            let cookie = cookie_for(&address(pinned.index(members.len())));
            request.headers.insert(
                header::COOKIE,
                cookie.parse().expect("hex cookie is a valid header"),
            );
        }

        // Weighted round robin never picks weight 0, whatever its health.
        let pickable = |upstream: &Arc<Upstream>, weight: u32| {
            upstream.is_available()
                && (policy != Policy::WeightedRoundRobin || weight > 0)
        };
        let any_pickable = upstreams
            .iter()
            .zip(&members)
            .any(|(upstream, (_, weight))| pickable(upstream, *weight));

        for _ in 0..16 {
            match pool.select(&request.context()) {
                Some(selection) => {
                    prop_assert!(
                        selection.upstream.is_available(),
                        "{policy:?} picked {} which is not available",
                        selection.upstream.address()
                    );
                }
                None => prop_assert!(
                    !any_pickable,
                    "{policy:?} picked nothing with an available backend"
                ),
            }
        }
    }
}
