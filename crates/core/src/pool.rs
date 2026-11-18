//! Runtime state of a load balancing group: members, health and selection.
//!
//! A [`Pool`] is built from an [`UpstreamGroup`] and can change at runtime:
//! backends can be added or removed through the admin API without touching
//! the ones that keep running, so their counters and health survive.

use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use http::header;
use http::{HeaderMap, Uri};
use serde::Serialize;
use sha2::Sha256;

use crate::config::{Backend, Fallback, PassiveHealth, Policy, UpstreamGroup};
use crate::random;

/// Default cookie name of the `cookie` policy, same as Caddy.
pub const DEFAULT_COOKIE_NAME: &str = "lb";

/// Error returned when changing pool membership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolError {
    /// A backend with this address is already a member.
    AlreadyExists(String),

    /// The address is not a valid `host:port`.
    InvalidAddress(String),
}

impl fmt::Display for PoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PoolError::AlreadyExists(address) => write!(f, "{address} is already in the pool"),
            PoolError::InvalidAddress(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PoolError {}

/// Request data used by the selection policies. Layer 4 proxies only fill
/// in the IP addresses.
#[derive(Debug, Clone, Copy)]
pub struct SelectContext<'a> {
    /// IP of the directly connected peer.
    pub peer_ip: IpAddr,

    /// IP of the real client, taken from trusted `X-Forwarded-For` values.
    pub client_ip: IpAddr,

    /// Request URI, for HTTP.
    pub uri: Option<&'a Uri>,

    /// Request headers, for HTTP.
    pub headers: Option<&'a HeaderMap>,

    /// True when the client connection uses TLS.
    pub secure: bool,
}

impl SelectContext<'_> {
    /// Context for a layer 4 connection.
    #[must_use]
    pub fn l4(peer_ip: IpAddr) -> Self {
        Self {
            peer_ip,
            client_ip: peer_ip,
            uri: None,
            headers: None,
            secure: false,
        }
    }
}

/// Result of a selection.
#[derive(Debug, Clone)]
pub struct Selection {
    /// Chosen backend.
    pub upstream: Arc<Upstream>,

    /// `Set-Cookie` value to send back, for the `cookie` policy.
    pub set_cookie: Option<String>,
}

impl Selection {
    fn plain(upstream: Arc<Upstream>) -> Self {
        Selection {
            upstream,
            set_cookie: None,
        }
    }
}

/// Snapshot of a backend, as reported by the admin API.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamStatus {
    /// Backend address.
    pub address: String,

    /// Configured weight.
    pub weight: u32,

    /// False when an active or passive check marked the backend down.
    pub healthy: bool,

    /// Healthy and below `unhealthy_request_count`.
    pub available: bool,

    /// Requests or connections in flight (Caddy `num_requests`).
    pub num_requests: usize,

    /// Requests or connections served since start.
    pub total_requests: u64,

    /// Failures remembered by passive health checks (Caddy `fails`).
    pub fails: u32,
}

/// Snapshot of a pool, as reported by the admin API.
#[derive(Debug, Clone, Serialize)]
pub struct PoolStatus {
    /// Pool name.
    pub name: String,

    /// Selection policy.
    pub policy: Policy,

    /// Members.
    pub upstreams: Vec<UpstreamStatus>,
}

/// One backend and its live counters.
#[derive(Debug)]
pub struct Upstream {
    address: String,
    weight: u32,
    address_hash: u64,
    dynamic: bool,
    cookie: Option<String>,
    active: AtomicUsize,
    total: AtomicU64,
    failure_total: AtomicU64,
    checked_healthy: AtomicBool,
    check_passes: AtomicU32,
    check_fails: AtomicU32,
    passive: Option<PassiveHealth>,
    fail_count: AtomicU32,
    failures: Mutex<VecDeque<Instant>>,
}

impl Upstream {
    fn new(backend: Backend, passive: Option<PassiveHealth>, cookie_secret: Option<&str>) -> Self {
        let cookie = cookie_secret.map(|secret| cookie_value(secret, &backend.address));
        Self {
            address_hash: hash_bytes(backend.address.as_bytes()),
            address: backend.address,
            weight: backend.weight,
            dynamic: false,
            cookie,
            active: AtomicUsize::new(0),
            total: AtomicU64::new(0),
            failure_total: AtomicU64::new(0),
            checked_healthy: AtomicBool::new(true),
            check_passes: AtomicU32::new(0),
            check_fails: AtomicU32::new(0),
            passive,
            fail_count: AtomicU32::new(0),
            failures: Mutex::new(VecDeque::new()),
        }
    }

    /// Address of the backend as configured (`host:port`, `https://host[:port]`,
    /// `unix//path`, ...).
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Configured weight.
    #[must_use]
    pub fn weight(&self) -> u32 {
        self.weight
    }

    /// Requests or connections in flight.
    #[must_use]
    pub fn active_requests(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// True unless active or passive health checks marked the backend down.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checked_healthy.load(Ordering::Relaxed) && self.passive_ok()
    }

    /// Healthy and not over the `unhealthy_request_count` limit.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.is_healthy() && !self.is_full()
    }

    fn is_full(&self) -> bool {
        match &self.passive {
            Some(passive) if passive.unhealthy_request_count > 0 => {
                self.active_requests() >= passive.unhealthy_request_count
            }
            _ => false,
        }
    }

    fn passive_ok(&self) -> bool {
        let Some(passive) = &self.passive else {
            return true;
        };

        // Fast path: no lock unless enough failures were recorded.
        if self.fail_count.load(Ordering::Relaxed) < passive.max_fails {
            return true;
        }

        self.prune_failures(passive) < passive.max_fails
    }

    fn prune_failures(&self, passive: &PassiveHealth) -> u32 {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        while failures
            .front()
            .is_some_and(|at| now.duration_since(*at) >= passive.fail_duration)
        {
            failures.pop_front();
        }
        let count = failures.len() as u32;
        self.fail_count.store(count, Ordering::Relaxed);
        count
    }

    /// Requests or connections served since start.
    #[must_use]
    pub fn total_requests(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Failures recorded since start, whether or not passive health checks
    /// are enabled.
    #[must_use]
    pub fn total_failures(&self) -> u64 {
        self.failure_total.load(Ordering::Relaxed)
    }

    /// Failures currently remembered by passive health checks.
    #[must_use]
    pub fn fails(&self) -> u32 {
        match &self.passive {
            Some(passive) => self.prune_failures(passive),
            None => 0,
        }
    }

    /// Records a failed request or connection for passive health checks.
    pub fn record_failure(&self) {
        self.failure_total.fetch_add(1, Ordering::Relaxed);
        let Some(passive) = &self.passive else {
            return;
        };
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        failures.push_back(Instant::now());
        self.fail_count
            .store(failures.len() as u32, Ordering::Relaxed);
        drop(failures);
        // Keep the queue bounded and the counter fresh.
        self.prune_failures(passive);
    }

    /// Feeds a completed HTTP response to passive health checks.
    pub fn record_response(&self, status: u16, latency: Duration) {
        let Some(passive) = &self.passive else {
            return;
        };

        let bad_status = passive
            .unhealthy_status
            .iter()
            .any(|matcher| matcher.matches(status));
        let too_slow = passive
            .unhealthy_latency
            .is_some_and(|limit| latency > limit);

        if bad_status || too_slow {
            self.record_failure();
        }
    }

    /// Feeds an active health check result. Returns the new health state
    /// when it changed.
    pub fn record_check(&self, passed: bool, passes: u32, fails: u32) -> Option<bool> {
        let healthy = self.checked_healthy.load(Ordering::Relaxed);

        if passed {
            self.check_fails.store(0, Ordering::Relaxed);
            let streak = self.check_passes.fetch_add(1, Ordering::Relaxed) + 1;
            if !healthy && streak >= passes {
                self.checked_healthy.store(true, Ordering::Relaxed);
                return Some(true);
            }
        } else {
            self.check_passes.store(0, Ordering::Relaxed);
            let streak = self.check_fails.fetch_add(1, Ordering::Relaxed) + 1;
            if healthy && streak >= fails {
                self.checked_healthy.store(false, Ordering::Relaxed);
                return Some(false);
            }
        }

        None
    }

    /// Counts a request in flight until the returned guard is dropped.
    #[must_use]
    pub fn start_request(self: &Arc<Self>) -> RequestGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        RequestGuard(Arc::clone(self))
    }

    /// Snapshot for the admin API.
    #[must_use]
    pub fn status(&self) -> UpstreamStatus {
        UpstreamStatus {
            address: self.address.clone(),
            weight: self.weight,
            healthy: self.is_healthy(),
            available: self.is_available(),
            num_requests: self.active_requests(),
            total_requests: self.total.load(Ordering::Relaxed),
            fails: self.fails(),
        }
    }
}

/// Keeps a request counted as in flight. See [`Upstream::start_request`].
#[derive(Debug)]
pub struct RequestGuard(Arc<Upstream>);

impl RequestGuard {
    /// Backend this request goes to.
    #[must_use]
    pub fn upstream(&self) -> &Arc<Upstream> {
        &self.0
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Immutable member list, swapped as a whole on changes.
#[derive(Debug, Default)]
struct Members {
    upstreams: Vec<Arc<Upstream>>,
    /// Smooth weighted round robin order, as indexes into `upstreams`.
    weighted_order: Vec<usize>,
}

impl Members {
    fn new(upstreams: Vec<Arc<Upstream>>) -> Self {
        let weighted_order = smooth_weighted_order(&upstreams);
        Self {
            upstreams,
            weighted_order,
        }
    }
}

/// Live load balancing group.
#[derive(Debug)]
pub struct Pool {
    name: String,
    settings: UpstreamGroup,
    members: RwLock<Arc<Members>>,
    cursor: AtomicUsize,
    allow_unix: bool,
}

impl Pool {
    /// Builds a pool from its configuration.
    pub fn new(name: impl Into<String>, group: &UpstreamGroup) -> Result<Self, PoolError> {
        let mut settings = group.clone();
        let backends = settings
            .expanded_backends()
            .map_err(PoolError::InvalidAddress)?;
        settings.backends.clear();

        let pool = Self {
            name: name.into(),
            settings,
            members: RwLock::new(Arc::default()),
            cursor: AtomicUsize::new(0),
            allow_unix: true,
        };

        let mut upstreams: Vec<Arc<Upstream>> = Vec::with_capacity(backends.len());
        for backend in backends {
            if upstreams
                .iter()
                .any(|upstream| upstream.address == backend.address)
            {
                return Err(PoolError::AlreadyExists(backend.address));
            }
            upstreams.push(Arc::new(pool.make_upstream(backend)));
        }
        *pool.members.write().unwrap_or_else(PoisonError::into_inner) =
            Arc::new(Members::new(upstreams));

        Ok(pool)
    }

    /// Makes [`Pool::add`] refuse `unix//` backends, for pools that serve UDP.
    #[must_use]
    pub fn without_unix(mut self) -> Self {
        self.allow_unix = false;
        self
    }

    fn make_upstream(&self, backend: Backend) -> Upstream {
        Upstream::new(
            backend,
            self.settings.passive.clone(),
            self.settings.cookie_secret(),
        )
    }

    fn members(&self) -> Arc<Members> {
        Arc::clone(&self.members.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Pool name. Inline groups get generated names such as `server[0]/api`.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Settings of this pool. The `backends` field is always empty, use
    /// [`Pool::upstreams`] for the live members.
    #[must_use]
    pub fn settings(&self) -> &UpstreamGroup {
        &self.settings
    }

    /// Current members.
    #[must_use]
    pub fn upstreams(&self) -> Vec<Arc<Upstream>> {
        self.members().upstreams.clone()
    }

    /// Configuration equivalent to the live state, members included.
    #[must_use]
    pub fn to_group(&self) -> UpstreamGroup {
        let mut group = self.settings.clone();
        group.backends = self
            .members()
            .upstreams
            .iter()
            .filter(|upstream| !upstream.dynamic)
            .map(|upstream| Backend {
                address: upstream.address.clone(),
                weight: upstream.weight,
            })
            .collect();
        group
    }

    /// Snapshot for the admin API.
    #[must_use]
    pub fn status(&self) -> PoolStatus {
        PoolStatus {
            name: self.name.clone(),
            policy: self.settings.policy,
            upstreams: self
                .members()
                .upstreams
                .iter()
                .map(|upstream| upstream.status())
                .collect(),
        }
    }

    /// Adds a backend. Existing members keep their state.
    pub fn add(&self, backend: Backend) -> Result<(), PoolError> {
        let mut expanded = UpstreamGroup::with_backends(vec![backend])
            .expanded_backends()
            .map_err(PoolError::InvalidAddress)?;

        let mut guard = self.members.write().unwrap_or_else(PoisonError::into_inner);
        let mut upstreams = guard.upstreams.clone();

        // Same rules as config validation: no unix sockets for UDP, and no
        // mixing of https:// and plain backends unless `tls` makes them all TLS.
        let is_https = |address: &str| address.starts_with("https://");
        let pool_https = self
            .settings
            .tls
            .is_none()
            .then(|| upstreams.first().map(|u| is_https(&u.address)))
            .flatten();
        for backend in &expanded {
            let address = &backend.address;
            if !self.allow_unix && address.starts_with("unix/") {
                return Err(PoolError::InvalidAddress(format!(
                    "{address}: unix// backends are not supported in pools used by udp servers"
                )));
            }
            if pool_https.is_some_and(|https| https != is_https(address)) {
                return Err(PoolError::InvalidAddress(format!(
                    "{address}: schemes can't be mixed, pool {} uses {} backends",
                    self.name,
                    if pool_https == Some(true) {
                        "https://"
                    } else {
                        "plain"
                    }
                )));
            }
        }
        for backend in expanded.drain(..) {
            if upstreams
                .iter()
                .any(|upstream| upstream.address == backend.address)
            {
                return Err(PoolError::AlreadyExists(backend.address));
            }
            upstreams.push(Arc::new(self.make_upstream(backend)));
        }
        *guard = Arc::new(Members::new(upstreams));
        Ok(())
    }

    /// Replaces the members that came from DNS with `addresses`. Static
    /// members are never touched, and members present in both lists keep
    /// their health and counters. Addresses equal to a static member are
    /// ignored.
    pub fn sync_dynamic(&self, addresses: Vec<Backend>) {
        let mut guard = self.members.write().unwrap_or_else(PoisonError::into_inner);
        let mut upstreams: Vec<Arc<Upstream>> = Vec::new();
        for upstream in &guard.upstreams {
            let keep = !upstream.dynamic
                || addresses.iter().any(|backend| {
                    backend.address == upstream.address && backend.weight == upstream.weight
                });
            if keep {
                upstreams.push(Arc::clone(upstream));
            }
        }
        for backend in addresses {
            if upstreams
                .iter()
                .any(|upstream| upstream.address == backend.address)
            {
                continue;
            }
            let mut upstream = self.make_upstream(backend);
            upstream.dynamic = true;
            upstreams.push(Arc::new(upstream));
        }
        let unchanged = upstreams.len() == guard.upstreams.len()
            && upstreams
                .iter()
                .zip(&guard.upstreams)
                .all(|(new, old)| Arc::ptr_eq(new, old));
        if !unchanged {
            *guard = Arc::new(Members::new(upstreams));
        }
    }

    /// Takes over the DNS members of `previous`, the same pool in the
    /// generation being replaced, so a reload does not start with an empty
    /// pool. Members keep their health and counters. Does nothing unless both
    /// pools resolve the same `a` or `srv` name.
    pub fn inherit_dynamic(&self, previous: &Pool) {
        let same = |pool: &Pool| {
            pool.settings
                .dynamic
                .as_ref()
                .map(|dynamic| (dynamic.a.clone(), dynamic.srv.clone()))
        };
        if same(self).is_none() || same(self) != same(previous) {
            return;
        }
        // Passive settings and cookie values are baked into each member: share
        // the old ones only when they still apply, otherwise rebuild them.
        let cookie_secret = |pool: &Pool| pool.settings.cookie_secret().map(str::to_owned);
        let reusable = self.settings.passive == previous.settings.passive
            && cookie_secret(self) == cookie_secret(previous);

        let mut guard = self.members.write().unwrap_or_else(PoisonError::into_inner);
        let mut upstreams = guard.upstreams.clone();
        for upstream in &previous.members().upstreams {
            if !upstream.dynamic || upstreams.iter().any(|u| u.address == upstream.address) {
                continue;
            }
            if reusable {
                upstreams.push(Arc::clone(upstream));
            } else {
                let mut rebuilt = self.make_upstream(Backend {
                    address: upstream.address.clone(),
                    weight: upstream.weight,
                });
                rebuilt.dynamic = true;
                upstreams.push(Arc::new(rebuilt));
            }
        }
        *guard = Arc::new(Members::new(upstreams));
    }

    /// Removes a backend. Returns false if it was not a member. Connections
    /// already open to it are not interrupted.
    pub fn remove(&self, address: &str) -> bool {
        let mut guard = self.members.write().unwrap_or_else(PoisonError::into_inner);
        let before = guard.upstreams.len();
        let upstreams: Vec<Arc<Upstream>> = guard
            .upstreams
            .iter()
            .filter(|upstream| upstream.address != address)
            .cloned()
            .collect();
        let removed = upstreams.len() != before;
        *guard = Arc::new(Members::new(upstreams));
        removed
    }

    /// Picks a backend for a request, or `None` if no backend is available.
    #[must_use]
    pub fn select(&self, context: &SelectContext<'_>) -> Option<Selection> {
        let members = self.members();
        let settings = &self.settings;
        let node = Node {
            policy: settings.policy,
            choose: settings.choose,
            key: settings.key.as_deref(),
            fallback: settings.fallback.as_ref(),
        };
        self.choose(node, &members, context)
    }

    /// Selects with `node`; when its policy cannot decide, with its fallback.
    fn choose(
        &self,
        node: Node<'_>,
        members: &Members,
        context: &SelectContext<'_>,
    ) -> Option<Selection> {
        let upstreams = &members.upstreams;
        let missing = || match node.fallback {
            Some(next) => self.choose(Node::from(next), members, context),
            None => pick_random(upstreams).map(Selection::plain),
        };

        let picked = match node.policy {
            Policy::Cookie => return self.select_cookie(node, members, context, missing),
            Policy::Random => pick_random(upstreams),
            Policy::RandomChoose => pick_random_choose(upstreams, node.choose),
            Policy::First => upstreams
                .iter()
                .find(|upstream| upstream.is_available())
                .cloned(),
            Policy::RoundRobin => {
                let order: Vec<usize> = (0..upstreams.len()).collect();
                self.rotate(upstreams, &order)
            }
            Policy::WeightedRoundRobin => self.rotate(upstreams, &members.weighted_order),
            Policy::LeastConn => pick_least_conn(upstreams),
            Policy::IpHash => pick_rendezvous(upstreams, ip_key(context.peer_ip).as_bytes()),
            Policy::ClientIpHash => {
                pick_rendezvous(upstreams, ip_key(context.client_ip).as_bytes())
            }
            Policy::UriHash => match context.uri.and_then(Uri::path_and_query) {
                Some(path) => pick_rendezvous(upstreams, path.as_str().as_bytes()),
                None => return missing(),
            },
            Policy::Query => match query_value(node.key, context) {
                Some(value) => pick_rendezvous(upstreams, value.as_bytes()),
                None => return missing(),
            },
            Policy::Header => match header_value(node.key, context) {
                Some(value) => pick_rendezvous(upstreams, value.as_bytes()),
                None => return missing(),
            },
        };
        picked.map(Selection::plain)
    }

    fn rotate(&self, upstreams: &[Arc<Upstream>], order: &[usize]) -> Option<Arc<Upstream>> {
        if order.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        (0..order.len())
            .map(|offset| &upstreams[order[(start + offset) % order.len()]])
            .find(|upstream| upstream.is_available())
            .cloned()
    }

    fn select_cookie(
        &self,
        node: Node<'_>,
        members: &Members,
        context: &SelectContext<'_>,
        missing: impl FnOnce() -> Option<Selection>,
    ) -> Option<Selection> {
        let name = node.key.unwrap_or(DEFAULT_COOKIE_NAME);

        if let Some(value) = context
            .headers
            .and_then(|headers| find_cookie(headers, name))
            && let Some(upstream) = members.upstreams.iter().find(|upstream| {
                upstream.cookie.as_deref() == Some(value) && upstream.is_available()
            })
        {
            return Some(Selection::plain(Arc::clone(upstream)));
        }

        let upstream = missing()?.upstream;
        let value = upstream.cookie.as_deref().unwrap_or_default();
        let secure = if context.secure {
            "; Secure; SameSite=None"
        } else {
            ""
        };
        let set_cookie = Some(format!("{name}={value}; Path=/{secure}"));
        Some(Selection {
            upstream,
            set_cookie,
        })
    }
}

/// Settings of one level of the policy chain.
#[derive(Clone, Copy)]
struct Node<'a> {
    policy: Policy,
    choose: usize,
    key: Option<&'a str>,
    fallback: Option<&'a Fallback>,
}

impl<'a> From<&'a Fallback> for Node<'a> {
    fn from(fallback: &'a Fallback) -> Self {
        Node {
            policy: fallback.policy,
            choose: fallback.choose,
            key: fallback.key.as_deref(),
            fallback: fallback.fallback.as_deref(),
        }
    }
}

fn query_value<'a>(key: Option<&str>, context: &SelectContext<'a>) -> Option<&'a str> {
    let key = key?;
    let query = context.uri?.query()?;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        (name == key && !value.is_empty()).then_some(value)
    })
}

fn header_value<'a>(key: Option<&str>, context: &SelectContext<'a>) -> Option<&'a str> {
    let value = context.headers?.get(key?)?.to_str().ok()?;
    (!value.is_empty()).then_some(value)
}

fn find_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}

fn cookie_value(secret: &str, address: &str) -> String {
    use hmac::KeyInit;

    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(address.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn ip_key(ip: IpAddr) -> String {
    ip.to_canonical().to_string()
}

fn pick_random(upstreams: &[Arc<Upstream>]) -> Option<Arc<Upstream>> {
    // Reservoir sampling over the available members.
    let mut chosen = None;
    let mut seen = 0;
    for upstream in upstreams.iter().filter(|upstream| upstream.is_available()) {
        seen += 1;
        if random::below(seen) == 0 {
            chosen = Some(upstream);
        }
    }
    chosen.cloned()
}

fn pick_random_choose(upstreams: &[Arc<Upstream>], choose: usize) -> Option<Arc<Upstream>> {
    let mut sample: Vec<&Arc<Upstream>> = Vec::with_capacity(choose);
    let mut seen = 0;
    for upstream in upstreams.iter().filter(|upstream| upstream.is_available()) {
        seen += 1;
        if sample.len() < choose {
            sample.push(upstream);
        } else {
            let slot = random::below(seen);
            if slot < choose {
                sample[slot] = upstream;
            }
        }
    }
    sample
        .into_iter()
        .min_by_key(|upstream| upstream.active_requests())
        .cloned()
}

fn pick_least_conn(upstreams: &[Arc<Upstream>]) -> Option<Arc<Upstream>> {
    let mut best: Option<&Arc<Upstream>> = None;
    let mut best_load = usize::MAX;
    let mut ties = 0;

    for upstream in upstreams.iter().filter(|upstream| upstream.is_available()) {
        let load = upstream.active_requests();
        if load < best_load {
            best = Some(upstream);
            best_load = load;
            ties = 1;
        } else if load == best_load {
            ties += 1;
            if random::below(ties) == 0 {
                best = Some(upstream);
            }
        }
    }

    best.cloned()
}

/// Highest random weight (rendezvous) hashing: the same key keeps mapping to
/// the same backend while it is available, and only keys owned by a removed
/// backend move when membership changes.
fn pick_rendezvous(upstreams: &[Arc<Upstream>], key: &[u8]) -> Option<Arc<Upstream>> {
    let key_hash = hash_bytes(key);
    upstreams
        .iter()
        .filter(|upstream| upstream.is_available())
        .max_by_key(|upstream| mix(key_hash ^ mix(upstream.address_hash)))
        .cloned()
}

/// FNV-1a, finished with [`mix`] for good bit dispersion.
fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix(hash)
}

/// SplitMix64 finalizer.
fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Nginx style smooth weighted round robin. Weights `[5, 1, 1]` produce
/// `[a, a, b, a, c, a, a]` instead of `[a, a, a, a, a, b, c]`.
fn smooth_weighted_order(upstreams: &[Arc<Upstream>]) -> Vec<usize> {
    let divisor = upstreams
        .iter()
        .map(|upstream| upstream.weight)
        .fold(0, gcd)
        .max(1);
    let weights: Vec<i64> = upstreams
        .iter()
        .map(|upstream| i64::from(upstream.weight / divisor))
        .collect();
    let total: i64 = weights.iter().sum();

    let mut current = vec![0_i64; weights.len()];
    let mut order = Vec::with_capacity(usize::try_from(total).unwrap_or_default());

    for _ in 0..total {
        for (value, weight) in current.iter_mut().zip(&weights) {
            *value += weight;
        }
        let (best, _) = current
            .iter()
            .enumerate()
            .max_by_key(|(index, value)| (**value, std::cmp::Reverse(*index)))
            .expect("total > 0 means at least one member");
        current[best] -= total;
        order.push(best);
    }

    order
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{DynamicUpstreams, PassiveHealth};

    fn group(policy: Policy, backends: &[(&str, u32)]) -> UpstreamGroup {
        let mut group = UpstreamGroup::with_backends(
            backends
                .iter()
                .map(|(address, weight)| Backend {
                    address: (*address).into(),
                    weight: *weight,
                })
                .collect(),
        );
        group.policy = policy;
        group
    }

    #[test]
    fn sync_dynamic_adds_removes_and_keeps_state() {
        let pool = Pool::new("p", &group(Policy::RoundRobin, &[("10.0.0.1:80", 1)])).unwrap();
        let dns = |addresses: &[&str]| addresses.iter().map(|a| Backend::new(*a)).collect();
        let addresses = |pool: &Pool| -> Vec<String> {
            pool.upstreams()
                .iter()
                .map(|u| u.address().to_owned())
                .collect()
        };

        pool.sync_dynamic(dns(&["10.0.1.1:80", "10.0.1.2:80", "10.0.0.1:80"]));
        assert_eq!(
            addresses(&pool),
            ["10.0.0.1:80", "10.0.1.1:80", "10.0.1.2:80"]
        );

        let kept = pool.upstreams()[1].clone();
        let _guard = kept.start_request();
        pool.sync_dynamic(dns(&["10.0.1.1:80", "10.0.1.3:80"]));
        assert_eq!(
            addresses(&pool),
            ["10.0.0.1:80", "10.0.1.1:80", "10.0.1.3:80"]
        );
        assert!(Arc::ptr_eq(&kept, &pool.upstreams()[1]));
        assert_eq!(pool.upstreams()[1].active_requests(), 1);

        // Dynamic members are not part of the persisted configuration.
        assert_eq!(pool.to_group().backends.len(), 1);

        // A changed weight replaces the member.
        pool.sync_dynamic(vec![Backend {
            address: "10.0.1.1:80".into(),
            weight: 4,
        }]);
        assert_eq!(pool.upstreams()[1].weight(), 4);
        assert!(!Arc::ptr_eq(&kept, &pool.upstreams()[1]));

        pool.sync_dynamic(Vec::new());
        assert_eq!(addresses(&pool), ["10.0.0.1:80"]);

        // A rebuilt pool inherits the DNS members and their state.
        let mut settings = group(Policy::RoundRobin, &[]);
        settings.dynamic = Some(DynamicUpstreams {
            a: Some("x:80".into()),
            srv: None,
            refresh: Duration::from_secs(1),
            resolvers: Vec::new(),
            versions: None,
        });
        let old = Pool::new("p", &settings).unwrap();
        old.sync_dynamic(dns(&["10.0.2.1:80"]));
        let new = Pool::new("p", &settings).unwrap();
        assert!(new.upstreams().is_empty());
        new.inherit_dynamic(&old);
        assert!(Arc::ptr_eq(&new.upstreams()[0], &old.upstreams()[0]));
        let mut changed = settings.clone();
        changed.passive = Some(PassiveHealth {
            fail_duration: Duration::from_secs(1),
            max_fails: 7,
            unhealthy_status: Vec::new(),
            unhealthy_latency: None,
            unhealthy_request_count: 0,
        });
        let rebuilt = Pool::new("p", &changed).unwrap();
        rebuilt.inherit_dynamic(&old);
        assert_eq!(rebuilt.upstreams()[0].address(), "10.0.2.1:80");
        assert!(!Arc::ptr_eq(&rebuilt.upstreams()[0], &old.upstreams()[0]));

        let mut cookie = settings.clone();
        cookie.policy = Policy::Cookie;
        cookie.secret = Some("a".into());
        let old_cookie = Pool::new("p", &cookie).unwrap();
        old_cookie.sync_dynamic(dns(&["10.0.2.1:80"]));
        let mut rotated = cookie.clone();
        rotated.secret = Some("b".into());
        let new_cookie = Pool::new("p", &rotated).unwrap();
        new_cookie.inherit_dynamic(&old_cookie);
        assert_eq!(new_cookie.upstreams().len(), 1);
        assert!(!Arc::ptr_eq(
            &new_cookie.upstreams()[0],
            &old_cookie.upstreams()[0]
        ));
        let same = Pool::new("p", &cookie).unwrap();
        same.inherit_dynamic(&old_cookie);
        assert!(Arc::ptr_eq(
            &same.upstreams()[0],
            &old_cookie.upstreams()[0]
        ));

        let other = Pool::new("p", &group(Policy::RoundRobin, &[])).unwrap();
        other.inherit_dynamic(&old);
        assert!(other.upstreams().is_empty());
    }

    fn context() -> SelectContext<'static> {
        SelectContext::l4("10.0.0.1".parse().unwrap())
    }

    fn pick(pool: &Pool, context: &SelectContext<'_>) -> String {
        pool.select(context).unwrap().upstream.address().to_owned()
    }

    #[test]
    fn weighted_round_robin_is_smooth_and_exact() {
        let pool = Pool::new(
            "t",
            &group(
                Policy::WeightedRoundRobin,
                &[("a:1", 5), ("b:1", 1), ("c:1", 1)],
            ),
        )
        .unwrap();
        let picks: Vec<String> = (0..7).map(|_| pick(&pool, &context())).collect();
        assert_eq!(picks, ["a:1", "a:1", "b:1", "a:1", "c:1", "a:1", "a:1"]);
    }

    #[test]
    fn weight_zero_is_never_picked() {
        let pool = Pool::new(
            "t",
            &group(Policy::WeightedRoundRobin, &[("a:1", 0), ("b:1", 1)]),
        )
        .unwrap();
        for _ in 0..10 {
            assert_eq!(pick(&pool, &context()), "b:1");
        }
    }

    #[test]
    fn round_robin_ignores_weights() {
        let pool = Pool::new("t", &group(Policy::RoundRobin, &[("a:1", 9), ("b:1", 1)])).unwrap();
        let picks: Vec<String> = (0..4).map(|_| pick(&pool, &context())).collect();
        assert_eq!(picks, ["a:1", "b:1", "a:1", "b:1"]);
    }

    #[test]
    fn first_fails_over_on_passive_failure() {
        let mut config = group(Policy::First, &[("a:1", 1), ("b:1", 1)]);
        config.passive = Some(PassiveHealth {
            fail_duration: Duration::from_secs(60),
            max_fails: 2,
            unhealthy_status: vec!["5xx".parse().unwrap()],
            unhealthy_latency: None,
            unhealthy_request_count: 0,
        });
        let pool = Pool::new("t", &config).unwrap();

        let first = pool.select(&context()).unwrap().upstream;
        assert_eq!(first.address(), "a:1");
        first.record_response(503, Duration::ZERO);
        assert_eq!(
            pick(&pool, &context()),
            "a:1",
            "one failure is below max_fails"
        );
        first.record_failure();
        assert_eq!(pick(&pool, &context()), "b:1");
        assert_eq!(first.status().fails, 2);
    }

    #[test]
    fn active_checks_need_streaks() {
        let pool = Pool::new("t", &group(Policy::First, &[("a:1", 1), ("b:1", 1)])).unwrap();
        let a = &pool.upstreams()[0];

        assert_eq!(a.record_check(false, 2, 2), None);
        assert_eq!(a.record_check(false, 2, 2), Some(false));
        assert_eq!(pick(&pool, &context()), "b:1");
        assert_eq!(a.record_check(true, 2, 2), None);
        assert_eq!(a.record_check(true, 2, 2), Some(true));
        assert_eq!(pick(&pool, &context()), "a:1");
    }

    #[test]
    fn least_conn_prefers_idle_backends() {
        let pool = Pool::new("t", &group(Policy::LeastConn, &[("a:1", 1), ("b:1", 1)])).unwrap();
        let busy = pool.select(&context()).unwrap().upstream;
        let _guard = busy.start_request();
        for _ in 0..10 {
            assert_ne!(pick(&pool, &context()), busy.address());
        }
    }

    #[test]
    fn request_count_limit_marks_backend_full() {
        let mut config = group(Policy::First, &[("a:1", 1), ("b:1", 1)]);
        config.passive = Some(PassiveHealth {
            fail_duration: Duration::from_secs(1),
            max_fails: 1,
            unhealthy_status: Vec::new(),
            unhealthy_latency: None,
            unhealthy_request_count: 1,
        });
        let pool = Pool::new("t", &config).unwrap();
        let guard = pool.select(&context()).unwrap().upstream.start_request();
        assert_eq!(pick(&pool, &context()), "b:1");
        drop(guard);
        assert_eq!(pick(&pool, &context()), "a:1");
    }

    #[test]
    fn hashes_are_sticky_and_spread() {
        let pool = Pool::new(
            "t",
            &group(Policy::IpHash, &[("a:1", 1), ("b:1", 1), ("c:1", 1)]),
        )
        .unwrap();
        let mut owners = HashMap::new();
        for last in 0..=255_u8 {
            let ip = IpAddr::from([10, 0, 0, last]);
            let owner = pick(&pool, &SelectContext::l4(ip));
            assert_eq!(
                owner,
                pick(&pool, &SelectContext::l4(ip)),
                "same IP must stick"
            );
            *owners.entry(owner).or_insert(0) += 1;
        }
        assert_eq!(
            owners.len(),
            3,
            "every backend should own some keys: {owners:?}"
        );
        for count in owners.values() {
            assert!(*count > 40, "distribution is too skewed: {owners:?}");
        }

        // Removing a backend only moves the keys it owned.
        let before: Vec<String> = (0..=255_u8)
            .map(|last| pick(&pool, &SelectContext::l4(IpAddr::from([10, 0, 0, last]))))
            .collect();
        assert!(pool.remove("c:1"));
        for (last, owner) in (0..=255_u8).zip(before) {
            if owner != "c:1" {
                assert_eq!(
                    owner,
                    pick(&pool, &SelectContext::l4(IpAddr::from([10, 0, 0, last])))
                );
            }
        }
    }

    #[test]
    fn header_and_query_policies_fall_back_without_key() {
        let mut config = group(Policy::Header, &[("a:1", 1), ("b:1", 1)]);
        config.key = Some("x-user".into());
        let pool = Pool::new("t", &config).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-user", "alice".parse().unwrap());
        let with_header = SelectContext {
            headers: Some(&headers),
            ..context()
        };
        let owner = pick(&pool, &with_header);
        for _ in 0..10 {
            assert_eq!(pick(&pool, &with_header), owner);
        }
        assert!(
            pool.select(&context()).is_some(),
            "fallback must pick something"
        );

        let mut config = group(Policy::Query, &[("a:1", 1), ("b:1", 1)]);
        config.key = Some("user".into());
        let pool = Pool::new("t", &config).unwrap();
        let uri: Uri = "/path?user=bob&x=1".parse().unwrap();
        let with_query = SelectContext {
            uri: Some(&uri),
            ..context()
        };
        let owner = pick(&pool, &with_query);
        for _ in 0..10 {
            assert_eq!(pick(&pool, &with_query), owner);
        }
    }

    #[test]
    fn nested_fallbacks_use_their_own_keys() {
        let fallback = |policy, key: Option<&str>, next: Option<Fallback>| Fallback {
            policy,
            choose: 2,
            key: key.map(Into::into),
            secret: None,
            fallback: next.map(Box::new),
        };
        let mut config = group(Policy::Header, &[("a:1", 1), ("b:1", 1), ("c:1", 1)]);
        config.key = Some("x-user".into());
        config.fallback = Some(fallback(
            Policy::Query,
            Some("user"),
            Some(fallback(Policy::First, None, None)),
        ));
        let pool = Pool::new("t", &config).unwrap();

        // No header, no query: the innermost `first` fallback decides.
        for _ in 0..5 {
            assert_eq!(pick(&pool, &context()), "a:1");
        }

        // No header, but the query fallback has its own key.
        let uri: Uri = "/?user=bob".parse().unwrap();
        let with_query = SelectContext {
            uri: Some(&uri),
            ..context()
        };
        let owner = pick(&pool, &with_query);
        for _ in 0..10 {
            assert_eq!(pick(&pool, &with_query), owner);
        }
    }

    #[test]
    fn cookie_fallback_sets_cookie_for_other_policies() {
        let mut config = group(Policy::Header, &[("a:1", 1), ("b:1", 1)]);
        config.key = Some("x-user".into());
        config.fallback = Some(Fallback {
            policy: Policy::Cookie,
            choose: 2,
            key: Some("sid".into()),
            secret: Some("s3cret".into()),
            fallback: None,
        });
        let pool = Pool::new("t", &config).unwrap();
        let selection = pool.select(&context()).unwrap();
        let cookie = selection.set_cookie.expect("cookie fallback sets a cookie");
        assert!(cookie.starts_with("sid="), "{cookie}");
    }

    #[test]
    fn cookie_policy_sets_and_honours_cookie() {
        let mut config = group(Policy::Cookie, &[("a:1", 1), ("b:1", 1)]);
        config.secret = Some("s3cret".into());
        let pool = Pool::new("t", &config).unwrap();

        let first = pool.select(&context()).unwrap();
        let set_cookie = first.set_cookie.expect("first request must get a cookie");
        assert!(set_cookie.starts_with("lb="));
        assert!(
            !set_cookie.contains(first.upstream.address()),
            "address must not leak"
        );

        let pair = set_cookie.split(';').next().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, format!("other=1; {pair}").parse().unwrap());
        let sticky = SelectContext {
            headers: Some(&headers),
            ..context()
        };
        for _ in 0..10 {
            let selection = pool.select(&sticky).unwrap();
            assert_eq!(selection.upstream.address(), first.upstream.address());
            assert!(selection.set_cookie.is_none());
        }
    }

    #[test]
    fn membership_changes_keep_state() {
        let pool = Pool::new("t", &group(Policy::RoundRobin, &[("a:1", 1)])).unwrap();
        let a = pool.upstreams()[0].clone();
        let _guard = a.start_request();

        pool.add(Backend::new("b:1")).unwrap();
        assert_eq!(
            pool.add(Backend::new("b:1")),
            Err(PoolError::AlreadyExists("b:1".into()))
        );
        assert_eq!(pool.upstreams()[0].active_requests(), 1);
        assert!(pool.remove("a:1"));
        assert!(!pool.remove("a:1"));
        assert_eq!(pick(&pool, &context()), "b:1");
        assert_eq!(pool.to_group().backends, vec![Backend::new("b:1")]);
    }

    #[test]
    fn empty_pool_selects_nothing() {
        let pool = Pool::new("t", &UpstreamGroup::default()).unwrap();
        assert!(pool.select(&context()).is_none());
    }
}
