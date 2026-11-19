//! Runtime state of a load balancing group: members, health and selection.
//!
//! A [`Pool`] is built from an [`UpstreamGroup`] and can change at runtime:
//! backends can be added or removed through the admin API without touching
//! the ones that keep running, so their counters and health survive.

use std::collections::VecDeque;
use std::fmt;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use http::header;
use http::{HeaderMap, Uri};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Notify;

use crate::config::{
    Backend, CircuitBreaker, Fallback, HashType, PassiveHealth, Policy, UpstreamGroup,
};
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

    /// State of the circuit breaker, for pools that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub circuit: Option<CircuitState>,

    /// State set by an operator: ready, draining or in maintenance.
    pub state: BackendState,
}

/// State of a backend set by an operator, like HAProxy's `set server state`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    /// Takes new requests.
    #[default]
    Ready,

    /// Takes no new requests, except those of its `cookie` sticky sessions.
    /// Requests and connections in flight finish.
    Drain,

    /// Takes no new requests at all. Requests and connections in flight
    /// finish.
    Maintenance,
}

impl BackendState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Drain,
            2 => Self::Maintenance,
            _ => Self::Ready,
        }
    }
}

impl fmt::Display for BackendState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ready => "ready",
            Self::Drain => "drain",
            Self::Maintenance => "maintenance",
        })
    }
}

/// State of the circuit breaker of a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    /// Requests flow and are counted.
    Closed,

    /// The backend is out of rotation.
    Open,

    /// The backend gets one trial request that decides the next state.
    HalfOpen,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Closed,
    Open { until: Instant },
    HalfOpen,
}

/// Counters of the current window of a [`Breaker`].
#[derive(Debug)]
struct BreakerWindow {
    phase: Phase,
    started: Instant,
    requests: u32,
    errors: u32,
    slow: u32,
}

/// Circuit breaker of one backend.
#[derive(Debug)]
struct Breaker {
    settings: CircuitBreaker,
    /// False while closed, so the request path takes no lock.
    tripped: AtomicBool,
    window: Mutex<BreakerWindow>,
}

impl Breaker {
    fn new(settings: CircuitBreaker) -> Self {
        Self {
            settings,
            tripped: AtomicBool::new(false),
            window: Mutex::new(BreakerWindow {
                phase: Phase::Closed,
                started: Instant::now(),
                requests: 0,
                errors: 0,
                slow: 0,
            }),
        }
    }

    /// Current state; an open breaker whose time is up becomes half open.
    fn state(&self) -> CircuitState {
        if !self.tripped.load(Ordering::Relaxed) {
            return CircuitState::Closed;
        }
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        Self::phase(&mut window, Instant::now())
    }

    fn phase(window: &mut BreakerWindow, now: Instant) -> CircuitState {
        if let Phase::Open { until } = window.phase
            && now >= until
        {
            window.phase = Phase::HalfOpen;
        }
        match window.phase {
            Phase::Closed => CircuitState::Closed,
            Phase::Open { .. } => CircuitState::Open,
            Phase::HalfOpen => CircuitState::HalfOpen,
        }
    }

    /// Counts a finished request. Returns true when a trial request closed
    /// the breaker.
    fn record(&self, error: bool, slow: bool) -> bool {
        let settings = &self.settings;
        let now = Instant::now();
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);

        let open = match Self::phase(&mut window, now) {
            // Late result of a request sent before the breaker opened.
            CircuitState::Open => return false,
            CircuitState::HalfOpen => error || slow,
            CircuitState::Closed => {
                // shortcut: fixed windows, a burst split over two windows goes
                // unseen; use sliding buckets if that matters.
                if now.duration_since(window.started) >= settings.window {
                    (window.requests, window.errors, window.slow) = (0, 0, 0);
                    window.started = now;
                }
                window.requests += 1;
                window.errors += u32::from(error);
                window.slow += u32::from(slow);

                let reaches =
                    |count: u32, ratio: f64| f64::from(count) >= ratio * f64::from(window.requests);
                let trips = window.requests >= settings.min_requests
                    && (settings
                        .error_ratio
                        .is_some_and(|ratio| reaches(window.errors, ratio))
                        || (settings.latency.is_some()
                            && reaches(window.slow, settings.latency_ratio)));
                if !trips {
                    return false;
                }
                trips
            }
        };

        (window.requests, window.errors, window.slow) = (0, 0, 0);
        window.started = now;
        window.phase = if open {
            Phase::Open {
                until: now + settings.open_duration,
            }
        } else {
            Phase::Closed
        };
        self.tripped.store(open, Ordering::Relaxed);
        !open
    }
}

/// Slow start of one backend: its share of the traffic grows linearly from
/// nothing to full over `duration`.
#[derive(Debug)]
struct Ramp {
    duration: Duration,
    /// False once the ramp is over, so the request path takes no lock.
    ramping: AtomicBool,
    started: Mutex<Instant>,
}

impl Ramp {
    fn new(duration: Duration) -> Self {
        Self {
            duration,
            ramping: AtomicBool::new(false),
            started: Mutex::new(Instant::now()),
        }
    }

    fn start(&self) {
        let mut started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
        *started = Instant::now();
        self.ramping.store(true, Ordering::Relaxed);
    }

    /// Draws whether one request may go to the backend: always once the ramp
    /// is over, before that with a chance equal to the elapsed share of it.
    fn admits(&self) -> bool {
        if !self.ramping.load(Ordering::Relaxed) {
            return true;
        }
        let started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
        let elapsed = started.elapsed();
        if elapsed >= self.duration {
            self.ramping.store(false, Ordering::Relaxed);
            return true;
        }
        let millis =
            |duration: Duration| usize::try_from(duration.as_millis()).unwrap_or(usize::MAX);
        random::below(millis(self.duration).max(1)) < millis(elapsed)
    }
}

/// Snapshot of a pool, as reported by the admin API.
#[derive(Debug, Clone, Serialize)]
pub struct PoolStatus {
    /// Pool name.
    pub name: String,

    /// Selection policy.
    pub policy: Policy,

    /// Requests waiting for a backend below `max_connections`.
    pub queued: usize,

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
    breaker: Option<Breaker>,
    ramp: Option<Ramp>,
    /// Requests in flight allowed at once, `0` for no limit.
    max_connections: usize,
    /// Wakes the queue of the pool when this backend can take a request.
    released: Arc<Notify>,
    /// [`BackendState`] as its discriminant.
    state: AtomicU8,
}

impl Upstream {
    fn new(
        backend: Backend,
        passive: Option<PassiveHealth>,
        breaker: Option<CircuitBreaker>,
        slow_start: Option<Duration>,
        cookie_secret: Option<&str>,
        max_connections: usize,
        released: Arc<Notify>,
    ) -> Self {
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
            breaker: breaker.map(Breaker::new),
            ramp: slow_start.map(Ramp::new),
            max_connections,
            released,
            state: AtomicU8::new(BackendState::Ready as u8),
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

    /// True unless active or passive health checks marked the backend down
    /// or its circuit breaker is open.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.checked_healthy.load(Ordering::Relaxed)
            && self.passive_ok()
            && self.circuit() != Some(CircuitState::Open)
    }

    /// Ready, healthy and not over the `unhealthy_request_count` limit.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.state() == BackendState::Ready && self.is_healthy() && !self.is_full()
    }

    /// Like [`Upstream::is_available`], but a draining backend still takes
    /// the requests of its sticky sessions.
    fn takes_sticky(&self) -> bool {
        self.state() != BackendState::Maintenance && self.is_healthy() && !self.is_full()
    }

    /// State set by an operator.
    #[must_use]
    pub fn state(&self) -> BackendState {
        BackendState::from_u8(self.state.load(Ordering::Relaxed))
    }

    /// Sets the operator state. A backend put back to ready ramps up again,
    /// for pools with slow start.
    pub fn set_state(&self, state: BackendState) {
        let before = BackendState::from_u8(self.state.swap(state as u8, Ordering::Relaxed));
        if state == BackendState::Ready && before != BackendState::Ready {
            self.start_ramp();
            self.released.notify_waiters();
        }
    }

    /// State of the circuit breaker, for pools that have one.
    #[must_use]
    pub fn circuit(&self) -> Option<CircuitState> {
        self.breaker.as_ref().map(Breaker::state)
    }

    /// Starts the slow start ramp, for pools that have one.
    fn start_ramp(&self) {
        if let Some(ramp) = &self.ramp {
            ramp.start();
        }
    }

    /// False when slow start turns away this request.
    fn ramp_admits(&self) -> bool {
        self.ramp.as_ref().is_none_or(Ramp::admits)
    }

    fn at_max_connections(&self) -> bool {
        self.max_connections > 0 && self.active_requests() >= self.max_connections
    }

    fn is_full(&self) -> bool {
        if self.at_max_connections() {
            return true;
        }
        // A half open breaker lets one trial request through at a time.
        if self.circuit() == Some(CircuitState::HalfOpen) && self.active_requests() > 0 {
            return true;
        }
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
        let before = self.fail_count.swap(count, Ordering::Relaxed);
        if before >= passive.max_fails && count < passive.max_fails {
            self.start_ramp();
        }
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

    /// Records a failed request or connection for passive health checks and
    /// the circuit breaker.
    pub fn record_failure(&self) {
        self.passive_failure();
        self.breaker_record(/* error= */ true, /* slow= */ false);
    }

    /// Records a connection that was established, for the circuit breaker of
    /// pools without HTTP responses to judge.
    pub fn record_success(&self) {
        self.breaker_record(/* error= */ false, /* slow= */ false);
    }

    fn breaker_record(&self, error: bool, slow: bool) {
        if let Some(breaker) = &self.breaker
            && breaker.record(error, slow)
        {
            self.start_ramp();
        }
    }

    fn passive_failure(&self) {
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

    /// Feeds a completed HTTP response to passive health checks and the
    /// circuit breaker.
    pub fn record_response(&self, status: u16, latency: Duration) {
        if let Some(breaker) = &self.breaker {
            let settings = &breaker.settings;
            let error = settings
                .error_status
                .iter()
                .any(|matcher| matcher.matches(status));
            let slow = settings.latency.is_some_and(|limit| latency > limit);
            self.breaker_record(error, slow);
        }

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
            self.passive_failure();
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
                self.start_ramp();
                self.released.notify_waiters();
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

    /// Counts a request in flight until the returned guard is dropped, even
    /// over `max_connections`.
    #[must_use]
    pub fn start_request(self: &Arc<Self>) -> RequestGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        self.total.fetch_add(1, Ordering::Relaxed);
        RequestGuard(Arc::clone(self))
    }

    /// Like [`Upstream::start_request`], but `None` when the backend is at
    /// `max_connections`. Concurrent callers never exceed the limit.
    #[must_use]
    pub fn try_start_request(self: &Arc<Self>) -> Option<RequestGuard> {
        if self.max_connections == 0 {
            return Some(self.start_request());
        }
        self.active
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
                (active < self.max_connections).then_some(active + 1)
            })
            .ok()?;
        self.total.fetch_add(1, Ordering::Relaxed);
        Some(RequestGuard(Arc::clone(self)))
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
            circuit: self.circuit(),
            state: self.state(),
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
        if self.0.max_connections > 0 {
            self.0.released.notify_one();
        }
    }
}

/// A request counted against the `retry_budget` of its pool, from its first
/// retry or hedge until it is dropped. See [`Pool::allow_retry`].
#[derive(Debug)]
pub struct RetryPermit(Arc<AtomicUsize>);

impl Drop for RetryPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Immutable member list, swapped as a whole on changes.
#[derive(Debug, Default)]
struct Members {
    upstreams: Vec<Arc<Upstream>>,
    /// Smooth weighted round robin order, as indexes into `upstreams`.
    weighted_order: Vec<usize>,
    /// Maglev lookup table, as indexes into `upstreams`. Empty unless the
    /// pool uses `hash_type = "maglev"`.
    maglev: Vec<u32>,
}

impl Members {
    fn new(upstreams: Vec<Arc<Upstream>>, hash_type: HashType) -> Self {
        let weights: Vec<u32> = upstreams.iter().map(|upstream| upstream.weight).collect();
        let weighted_order = smooth_weighted_order(&weights);
        let maglev = match hash_type {
            HashType::Rendezvous => Vec::new(),
            HashType::Maglev => {
                let hashes: Vec<u64> = upstreams.iter().map(|u| u.address_hash).collect();
                maglev_table(&hashes, MAGLEV_TABLE_SIZE)
            }
        };
        Self {
            upstreams,
            weighted_order,
            maglev,
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
    /// Requests waiting in [`Pool::acquire`].
    queued: AtomicUsize,
    released: Arc<Notify>,
    /// Requests holding a [`RetryPermit`].
    retrying: Arc<AtomicUsize>,
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
            queued: AtomicUsize::new(0),
            released: Arc::default(),
            retrying: Arc::default(),
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
            Arc::new(Members::new(upstreams, pool.settings.hash_type));

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
            self.settings.circuit_breaker.clone(),
            self.settings.slow_start,
            self.settings.cookie_secret(),
            self.settings.max_connections,
            Arc::clone(&self.released),
        )
    }

    fn members(&self) -> Arc<Members> {
        Arc::clone(&self.members.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// True when at least one member can take new requests.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.members()
            .upstreams
            .iter()
            .any(|upstream| upstream.is_available())
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
            queued: self.queued.load(Ordering::Relaxed),
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
            let upstream = self.make_upstream(backend);
            upstream.start_ramp();
            upstreams.push(Arc::new(upstream));
        }
        *guard = Arc::new(Members::new(upstreams, self.settings.hash_type));
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
            upstream.start_ramp();
            upstreams.push(Arc::new(upstream));
        }
        let unchanged = upstreams.len() == guard.upstreams.len()
            && upstreams
                .iter()
                .zip(&guard.upstreams)
                .all(|(new, old)| Arc::ptr_eq(new, old));
        if !unchanged {
            *guard = Arc::new(Members::new(upstreams, self.settings.hash_type));
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
        // Passive and breaker settings and cookie values are baked into each member: share
        // the old ones only when they still apply, otherwise rebuild them.
        let cookie_secret = |pool: &Pool| pool.settings.cookie_secret().map(str::to_owned);
        let reusable = self.settings.passive == previous.settings.passive
            && self.settings.circuit_breaker == previous.settings.circuit_breaker
            && self.settings.slow_start == previous.settings.slow_start
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
        *guard = Arc::new(Members::new(upstreams, self.settings.hash_type));
    }

    /// Copies the operator state of the members of `previous`, the same pool
    /// in the generation being replaced, to the members with the same
    /// address, so a reload does not put drained backends back to work.
    pub fn inherit_states(&self, previous: &Pool) {
        let members = self.members();
        for old in &previous.members().upstreams {
            if let Some(upstream) = members.upstreams.iter().find(|u| u.address == old.address) {
                upstream
                    .state
                    .store(old.state.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
    }

    /// Sets the operator state of the member at `address`. Returns false if it
    /// is not a member.
    pub fn set_state(&self, address: &str, state: BackendState) -> bool {
        let members = self.members();
        let Some(upstream) = members.upstreams.iter().find(|u| u.address == address) else {
            return false;
        };
        upstream.set_state(state);
        true
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
        *guard = Arc::new(Members::new(upstreams, self.settings.hash_type));
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

    /// Selects a backend and counts a request in flight on it, within its
    /// `max_connections`. `None` if no backend is available.
    #[must_use]
    pub fn try_acquire(&self, context: &SelectContext<'_>) -> Option<(Selection, RequestGuard)> {
        loop {
            let selection = self.select(context)?;
            // Lost only to a concurrent request that took the last slot.
            if let Some(guard) = selection.upstream.try_start_request() {
                return Some((selection, guard));
            }
        }
    }

    /// Like [`Pool::try_acquire`], but when the usable backends are all at
    /// `max_connections` the request waits in the queue of the pool until
    /// one of them is free. `None` if no backend is available for another
    /// reason, or if `queue_size` requests are waiting already. The caller
    /// bounds the wait (`queue_timeout`) by dropping the future.
    pub async fn acquire(&self, context: &SelectContext<'_>) -> Option<(Selection, RequestGuard)> {
        if let Some(acquired) = self.try_acquire(context) {
            return Some(acquired);
        }
        if self.queued.fetch_add(1, Ordering::Relaxed) >= self.settings.queue_size {
            self.queued.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        let _queued = Queued(self);

        loop {
            let mut released = std::pin::pin!(self.released.notified());
            // Registered before looking, so a release in between is not lost.
            released.as_mut().enable();
            if let Some(acquired) = self.try_acquire(context) {
                return Some(acquired);
            }
            if !self.is_saturated() {
                // Nothing to wait for; the other waiters must see that too.
                self.released.notify_one();
                return None;
            }
            released.await;
        }
    }

    /// Whether a request may be sent again, as a retry or as a hedge, within
    /// the `retry_budget` of the pool. A request counts against the budget
    /// from its first retry until `permit` is dropped, so one that holds a
    /// permit may always go on. Always true without a budget.
    #[must_use]
    pub fn allow_retry(&self, permit: &mut Option<RetryPermit>) -> bool {
        let Some(budget) = &self.settings.retry_budget else {
            return true;
        };
        if permit.is_some() {
            return true;
        }
        let active: usize = self
            .members()
            .upstreams
            .iter()
            .map(|upstream| upstream.active_requests())
            .sum();
        if self.retrying.fetch_add(1, Ordering::Relaxed) >= budget.limit(active) {
            self.retrying.fetch_sub(1, Ordering::Relaxed);
            return false;
        }
        *permit = Some(RetryPermit(Arc::clone(&self.retrying)));
        true
    }

    /// True when a member would take requests if it were not at
    /// `max_connections`.
    fn is_saturated(&self) -> bool {
        self.members().upstreams.iter().any(|upstream| {
            upstream.state() == BackendState::Ready
                && upstream.is_healthy()
                && upstream.at_max_connections()
        })
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
            None => slow_start(|ready| pick_random(upstreams, ready)).map(Selection::plain),
        };

        let hash = |key: &[u8]| pick_hash(members, key, self.settings.hash_balance_factor);

        let picked = match node.policy {
            Policy::Cookie => return self.select_cookie(node, members, context, missing),
            Policy::Random => slow_start(|ready| pick_random(upstreams, ready)),
            Policy::RandomChoose => {
                slow_start(|ready| pick_random_choose(upstreams, node.choose, ready))
            }
            Policy::First => {
                slow_start(|ready| upstreams.iter().find(|upstream| ready(upstream)).cloned())
            }
            Policy::RoundRobin => {
                slow_start(|ready| self.rotate(upstreams, upstreams.len(), |turn| turn, ready))
            }
            Policy::WeightedRoundRobin => {
                let order = &members.weighted_order;
                slow_start(|ready| self.rotate(upstreams, order.len(), |turn| order[turn], ready))
            }
            Policy::LeastConn => slow_start(|ready| pick_least_conn(upstreams, ready)),
            Policy::IpHash => hash(ip_key(context.peer_ip).as_bytes()),
            Policy::ClientIpHash => hash(ip_key(context.client_ip).as_bytes()),
            Policy::UriHash => match context.uri.and_then(Uri::path_and_query) {
                Some(path) => hash(path.as_str().as_bytes()),
                None => return missing(),
            },
            Policy::Query => match query_value(node.key, context) {
                Some(value) => hash(value.as_bytes()),
                None => return missing(),
            },
            Policy::Header => match header_value(node.key, context) {
                Some(value) => hash(value.as_bytes()),
                None => return missing(),
            },
        };
        picked.map(Selection::plain)
    }

    /// First available member of a cycle of `turns` turns, starting one turn
    /// later on every call. `member` is the index of the member of a turn.
    fn rotate(
        &self,
        upstreams: &[Arc<Upstream>],
        turns: usize,
        member: impl Fn(usize) -> usize,
        ready: Ready<'_>,
    ) -> Option<Arc<Upstream>> {
        if turns == 0 {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        (0..turns)
            .map(|offset| &upstreams[member((start + offset) % turns)])
            .find(|upstream| ready(upstream))
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
                upstream.cookie.as_deref() == Some(value) && upstream.takes_sticky()
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

/// A request counted in the queue of a pool.
struct Queued<'a>(&'a Pool);

impl Drop for Queued<'_> {
    fn drop(&mut self) {
        self.0.queued.fetch_sub(1, Ordering::Relaxed);
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

/// Test of whether a member may take the request being placed.
type Ready<'a> = &'a dyn Fn(&Upstream) -> bool;

/// Runs a policy with slow start: members still ramping up are turned away
/// at random. When that leaves no member, the policy runs again without
/// slow start, so a pool of ramping members still answers.
fn slow_start(pick: impl Fn(Ready<'_>) -> Option<Arc<Upstream>>) -> Option<Arc<Upstream>> {
    pick(&|upstream| upstream.is_available() && upstream.ramp_admits())
        .or_else(|| pick(&Upstream::is_available))
}

fn pick_random(upstreams: &[Arc<Upstream>], ready: Ready<'_>) -> Option<Arc<Upstream>> {
    // Reservoir sampling over the ready members.
    let mut chosen = None;
    let mut seen = 0;
    for upstream in upstreams.iter().filter(|upstream| ready(upstream)) {
        seen += 1;
        if random::below(seen) == 0 {
            chosen = Some(upstream);
        }
    }
    chosen.cloned()
}

fn pick_random_choose(
    upstreams: &[Arc<Upstream>],
    choose: usize,
    ready: Ready<'_>,
) -> Option<Arc<Upstream>> {
    let mut sample: Vec<&Arc<Upstream>> = Vec::with_capacity(choose);
    let mut seen = 0;
    for upstream in upstreams.iter().filter(|upstream| ready(upstream)) {
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

fn pick_least_conn(upstreams: &[Arc<Upstream>], ready: Ready<'_>) -> Option<Arc<Upstream>> {
    let mut best: Option<&Arc<Upstream>> = None;
    let mut best_load = usize::MAX;
    let mut ties = 0;

    for upstream in upstreams.iter().filter(|upstream| ready(upstream)) {
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

/// Backend of a key for the hash policies. The same key keeps mapping to
/// the same backend while it is available, and only keys owned by a removed
/// backend move when membership changes.
///
/// The owner of a key is its entry in the Maglev table when the pool has
/// one, otherwise the member with the highest random weight (rendezvous
/// hashing). When the owner is unavailable, or at the load bound of
/// `balance_factor`, the key goes to the next member in rendezvous order.
fn pick_hash(members: &Members, key: &[u8], balance_factor: Option<u32>) -> Option<Arc<Upstream>> {
    let key_hash = hash_bytes(key);
    let upstreams = &members.upstreams;

    // Consistent hashing with bounded loads: no backend takes more than
    // `balance_factor` percent of the average load, this request included.
    let bound = balance_factor.map(|factor| {
        let available = upstreams.iter().filter(|upstream| upstream.is_available());
        let (count, load) = available.fold((0, 1), |(count, load), upstream| {
            (count + 1, load + upstream.active_requests())
        });
        let factor = usize::try_from(factor).unwrap_or(usize::MAX);
        load.saturating_mul(factor).div_ceil(100 * count.max(1))
    });
    let fits = |upstream: &Upstream| bound.is_none_or(|bound| upstream.active_requests() < bound);

    let owner = match members.maglev.len() {
        0 => None,
        size => usize::try_from(key_hash % (size as u64))
            .ok()
            .and_then(|slot| usize::try_from(members.maglev[slot]).ok()),
    };
    if let Some(upstream) = owner.map(|index| &upstreams[index])
        && upstream.is_available()
        && fits(upstream)
    {
        return Some(Arc::clone(upstream));
    }

    // Members over the bound stay candidates, after the others: the loads
    // move while they are read, so the bound can leave nobody.
    upstreams
        .iter()
        .filter(|upstream| upstream.is_available())
        .max_by_key(|upstream| (fits(upstream), mix(key_hash ^ mix(upstream.address_hash))))
        .cloned()
}

/// Entries of the Maglev table, a prime as the algorithm requires. Same
/// default as Envoy.
const MAGLEV_TABLE_SIZE: usize = 65_537;

/// Maglev lookup table (Eisenbud et al., NSDI 2016, section 3.4) of `size`
/// entries, a prime, over members given by their hashes: each entry is the
/// index of a member. Members take turns claiming the next free entry of
/// their own permutation of the table, so every member owns an almost equal
/// share, and a membership change moves few entries besides those of the
/// member that left. Empty without members.
fn maglev_table(hashes: &[u64], size: usize) -> Vec<u32> {
    const FREE: u32 = u32::MAX;

    let reduce = |hash: u64, modulus: usize| {
        usize::try_from(hash % (modulus as u64)).expect("a remainder of a usize fits in usize")
    };
    // Next entry each member wants, and the step to the one after.
    let mut wanted: Vec<(usize, usize)> = hashes
        .iter()
        .map(|hash| (reduce(*hash, size), reduce(mix(*hash), size - 1) + 1))
        .collect();
    let mut table = vec![FREE; if hashes.is_empty() { 0 } else { size }];
    let mut free = table.len();

    while free > 0 {
        for (member, (entry, step)) in (0_u32..).zip(&mut wanted) {
            while table[*entry] != FREE {
                *entry = (*entry + *step) % size;
            }
            table[*entry] = member;
            free -= 1;
            if free == 0 {
                break;
            }
        }
    }
    table
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
/// Nginx style smooth weighted round robin: one cycle of picks, as indexes
/// into `weights`, where index `i` appears `weights[i] / gcd` times and the
/// picks of each index are spread over the cycle. Empty when every weight is
/// zero.
#[must_use]
pub fn smooth_weighted_order(weights: &[u32]) -> Vec<usize> {
    let divisor = weights.iter().copied().fold(0, gcd).max(1);
    let weights: Vec<i64> = weights
        .iter()
        .map(|weight| i64::from(weight / divisor))
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
    use crate::config::{DynamicUpstreams, PassiveHealth, RetryBudget};

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

    fn limited(max_connections: usize, queue_size: usize) -> Pool {
        let mut config = group(Policy::First, &[("a:1", 1), ("b:1", 1)]);
        config.max_connections = max_connections;
        config.queue_size = queue_size;
        Pool::new("t", &config).unwrap()
    }

    /// Polls a future once: the queue needs no runtime.
    fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
        future.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
    }

    #[test]
    fn max_connections_spills_over_then_refuses() {
        let pool = limited(1, 0);
        let context = context();
        let (first, _a) = pool.try_acquire(&context).unwrap();
        let (second, b) = pool.try_acquire(&context).unwrap();
        assert_eq!(
            (first.upstream.address(), second.upstream.address()),
            ("a:1", "b:1")
        );
        assert!(pool.try_acquire(&context).is_none());
        assert!(second.upstream.try_start_request().is_none());
        // Without a queue, `acquire` refuses at once too.
        let mut refused = std::pin::pin!(pool.acquire(&context));
        assert!(matches!(
            poll(refused.as_mut()),
            std::task::Poll::Ready(None)
        ));

        drop(b);
        assert_eq!(
            pool.try_acquire(&context).unwrap().0.upstream.address(),
            "b:1"
        );
    }

    #[test]
    fn queue_waits_for_a_release_and_is_bounded() {
        let pool = limited(1, 1);
        let context = context();
        let (_, a) = pool.try_acquire(&context).unwrap();
        let (_, _b) = pool.try_acquire(&context).unwrap();

        let mut waiting = std::pin::pin!(pool.acquire(&context));
        assert!(poll(waiting.as_mut()).is_pending());
        assert_eq!(pool.status().queued, 1);
        // The queue holds one request: the next is refused.
        let mut over = std::pin::pin!(pool.acquire(&context));
        assert!(matches!(poll(over.as_mut()), std::task::Poll::Ready(None)));

        drop(a);
        let std::task::Poll::Ready(Some((selection, _guard))) = poll(waiting.as_mut()) else {
            panic!("the released slot goes to the queued request");
        };
        assert_eq!(selection.upstream.address(), "a:1");
        assert_eq!(pool.status().queued, 0);
    }

    #[test]
    fn queue_does_not_wait_for_down_backends() {
        let pool = limited(1, 4);
        let context = context();
        for upstream in pool.upstreams() {
            upstream.set_state(BackendState::Maintenance);
        }
        let mut refused = std::pin::pin!(pool.acquire(&context));
        assert!(matches!(
            poll(refused.as_mut()),
            std::task::Poll::Ready(None)
        ));
    }

    #[test]
    fn leaving_the_queue_frees_its_place() {
        let pool = limited(1, 1);
        let context = context();
        let _guards = [pool.try_acquire(&context), pool.try_acquire(&context)];
        {
            let mut waiting = std::pin::pin!(pool.acquire(&context));
            assert!(poll(waiting.as_mut()).is_pending());
        }
        assert_eq!(pool.status().queued, 0);
    }

    #[test]
    fn retry_budget_bounds_concurrent_retries() {
        let mut group = group(Policy::RoundRobin, &[("10.0.0.1:80", 1)]);
        assert!(
            Pool::new("p", &group).unwrap().allow_retry(&mut None),
            "no budget, no bound"
        );

        group.retry_budget = Some(RetryBudget {
            percent: 50.0,
            min_retries: 1,
        });
        let pool = Pool::new("p", &group).unwrap();
        let (mut first, mut second, mut third) = (None, None, None);
        assert!(pool.allow_retry(&mut first));
        assert!(pool.allow_retry(&mut first), "counted once per request");
        assert!(!pool.allow_retry(&mut second), "min_retries is used up");
        assert!(second.is_none());

        // Four requests in flight: half as many may retry.
        let guards: Vec<RequestGuard> = (0..4)
            .map(|_| pool.try_acquire(&context()).unwrap().1)
            .collect();
        assert!(pool.allow_retry(&mut second));
        assert!(!pool.allow_retry(&mut third));
        drop(guards);

        drop(first);
        drop(second);
        assert!(pool.allow_retry(&mut third), "finished retries give back");
    }

    #[test]
    fn maglev_table_gives_equal_shares() {
        let hashes: Vec<u64> = (0..7_u8).map(|member| hash_bytes(&[member])).collect();
        let table = maglev_table(&hashes, MAGLEV_TABLE_SIZE);
        assert_eq!(table.len(), MAGLEV_TABLE_SIZE);
        let mut shares = [0_usize; 7];
        for member in &table {
            shares[usize::try_from(*member).unwrap()] += 1;
        }
        // Members claim entries in turns, so shares differ by one at most.
        let fair = MAGLEV_TABLE_SIZE / 7;
        assert!(
            shares.iter().all(|share| (fair..=fair + 1).contains(share)),
            "{shares:?}"
        );

        // Members with the same hash want the same entries and still end.
        assert_eq!(maglev_table(&[1, 1, 1], 13).len(), 13);
        assert!(maglev_table(&[], 13).is_empty());
    }

    #[test]
    fn maglev_is_sticky_and_moves_few_keys() {
        let mut group = group(
            Policy::IpHash,
            &[("a:1", 1), ("b:1", 1), ("c:1", 1), ("d:1", 1)],
        );
        group.hash_type = HashType::Maglev;
        let pool = Pool::new("t", &group).unwrap();
        let owners = |pool: &Pool| -> Vec<String> {
            (0..2000_u16)
                .map(|key| {
                    let [high, low] = key.to_be_bytes();
                    pick(pool, &SelectContext::l4(IpAddr::from([10, 0, high, low])))
                })
                .collect()
        };

        let before = owners(&pool);
        assert_eq!(before, owners(&pool), "same IP must stick");
        for address in ["a:1", "b:1", "c:1", "d:1"] {
            let share = before.iter().filter(|owner| *owner == address).count();
            assert!(
                (400..=600).contains(&share),
                "{address} owns {share} of 2000"
            );
        }

        // An unavailable owner hands its keys over and takes them back.
        pool.set_state("d:1", BackendState::Maintenance);
        let without: Vec<String> = owners(&pool);
        for (old, new) in before.iter().zip(&without) {
            assert_ne!(new, "d:1");
            assert!(old == "d:1" || old == new, "{old} lost a key to {new}");
        }
        pool.set_state("d:1", BackendState::Ready);
        assert_eq!(before, owners(&pool));

        // Removing a member rebuilds the table: besides its own keys, only
        // a few move.
        assert!(pool.remove("d:1"));
        let moved = before
            .iter()
            .zip(owners(&pool))
            .filter(|(old, new)| *old != "d:1" && **old != *new)
            .count();
        assert!(
            moved < 100,
            "{moved} of 2000 keys moved between other backends"
        );
    }

    #[test]
    fn bounded_loads_move_keys_off_busy_backends() {
        for hash_type in [HashType::Rendezvous, HashType::Maglev] {
            let mut group = group(Policy::IpHash, &[("a:1", 1), ("b:1", 1), ("c:1", 1)]);
            group.hash_type = hash_type;
            let context = SelectContext::l4(IpAddr::from([10, 0, 0, 1]));

            // Without a bound the key stays, whatever the load.
            let unbounded = Pool::new("t", &group).unwrap();
            let owner = unbounded.select(&context).unwrap().upstream;
            let _busy: Vec<RequestGuard> = (0..4).map(|_| owner.start_request()).collect();
            assert_eq!(pick(&unbounded, &context), owner.address());

            group.hash_balance_factor = Some(125);
            let pool = Pool::new("t", &group).unwrap();
            let owner = pool.select(&context).unwrap().upstream;
            assert_eq!(owner.address(), pick(&unbounded, &context));

            // 2 of 2 requests in flight are on the owner: with this one the
            // bound is 125% of 3 / 3, rounded up to 2.
            let mut busy = vec![owner.start_request(), owner.start_request()];
            let second = pool.select(&context).unwrap().upstream;
            assert_ne!(second.address(), owner.address());
            assert_eq!(
                pick(&pool, &context),
                second.address(),
                "the next one is stable"
            );

            // With 3 requests on each of the two, the bound is 125% of 7 / 3,
            // rounded up to 3: the last backend takes over.
            busy.push(owner.start_request());
            busy.extend((0..3).map(|_| second.start_request()));
            let third = pick(&pool, &context);
            assert!(third != owner.address() && third != second.address());

            // An unavailable backend does not count in the average.
            pool.set_state(&third, BackendState::Maintenance);
            assert_eq!(pick(&pool, &context), owner.address());
            pool.set_state(&third, BackendState::Ready);

            drop(busy);
            assert_eq!(pick(&pool, &context), owner.address());
        }
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
    fn drained_and_maintenance_backends_take_no_new_requests() {
        for policy in [Policy::RoundRobin, Policy::LeastConn, Policy::IpHash] {
            let pool = Pool::new("t", &group(policy, &[("a:1", 1), ("b:1", 1)])).unwrap();
            assert!(pool.set_state("a:1", BackendState::Drain));
            for _ in 0..10 {
                assert_eq!(pick(&pool, &context()), "b:1", "{policy:?}");
            }
            assert!(pool.set_state("b:1", BackendState::Maintenance));
            assert!(pool.select(&context()).is_none(), "{policy:?}");
            assert!(!pool.is_available());

            assert!(pool.set_state("a:1", BackendState::Ready));
            assert_eq!(pick(&pool, &context()), "a:1", "{policy:?}");
        }

        let pool = Pool::new("t", &group(Policy::RoundRobin, &[("a:1", 1)])).unwrap();
        assert!(!pool.set_state("z:1", BackendState::Drain));
        let status = pool.status();
        assert_eq!(status.upstreams[0].state, BackendState::Ready);
        pool.set_state("a:1", BackendState::Drain);
        let status = pool.status();
        assert_eq!(status.upstreams[0].state, BackendState::Drain);
        assert!(status.upstreams[0].healthy);
        assert!(!status.upstreams[0].available);
    }

    #[test]
    fn drained_backend_keeps_its_sticky_sessions_until_maintenance() {
        let mut config = group(Policy::Cookie, &[("a:1", 1), ("b:1", 1)]);
        config.secret = Some("s3cret".into());
        let pool = Pool::new("t", &config).unwrap();
        let first = pool.select(&context()).unwrap();
        let address = first.upstream.address().to_owned();
        let pair = first.set_cookie.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            pair.split(';').next().unwrap().parse().unwrap(),
        );
        let sticky = SelectContext {
            headers: Some(&headers),
            ..context()
        };

        pool.set_state(&address, BackendState::Drain);
        assert_eq!(pool.select(&sticky).unwrap().upstream.address(), address);
        for _ in 0..10 {
            assert_ne!(pick(&pool, &context()), address, "new sessions avoid it");
        }

        pool.set_state(&address, BackendState::Maintenance);
        let moved = pool.select(&sticky).unwrap();
        assert_ne!(moved.upstream.address(), address);
        assert!(
            moved.set_cookie.is_some(),
            "the session moves to another backend"
        );
    }

    #[test]
    fn ready_again_ramps_up_with_slow_start() {
        let mut config = group(Policy::RoundRobin, &[("a:1", 1), ("b:1", 1)]);
        config.slow_start = Some(Duration::from_secs(3600));
        let pool = Pool::new("t", &config).unwrap();
        pool.set_state("a:1", BackendState::Maintenance);
        pool.set_state("a:1", BackendState::Ready);
        let to_a = (0..50).filter(|_| pick(&pool, &context()) == "a:1").count();
        assert!(
            to_a <= 3,
            "{to_a} of 50 requests went to the ramping backend"
        );
    }

    #[test]
    fn reload_keeps_operator_states() {
        let config = group(Policy::RoundRobin, &[("a:1", 1), ("b:1", 1)]);
        let old = Pool::new("t", &config).unwrap();
        old.set_state("a:1", BackendState::Drain);
        let new = Pool::new("t", &config).unwrap();
        new.inherit_states(&old);
        assert_eq!(new.upstreams()[0].state(), BackendState::Drain);
        assert_eq!(new.upstreams()[1].state(), BackendState::Ready);
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

    fn breaker_pool(settings: CircuitBreaker) -> (Pool, Arc<Upstream>) {
        let mut group = group(Policy::First, &[("10.0.0.1:80", 1), ("10.0.0.2:80", 1)]);
        group.circuit_breaker = Some(settings);
        let pool = Pool::new("p", &group).unwrap();
        let first = Arc::clone(&pool.upstreams()[0]);
        (pool, first)
    }

    fn breaker(error_ratio: Option<f64>, latency: Option<Duration>) -> CircuitBreaker {
        CircuitBreaker {
            error_ratio,
            error_status: vec![crate::status::StatusMatcher::Class(5)],
            latency,
            latency_ratio: 0.5,
            window: Duration::from_secs(60),
            min_requests: 4,
            open_duration: Duration::from_millis(40),
        }
    }

    fn selected(pool: &Pool) -> String {
        let context = SelectContext::l4(IpAddr::from([127, 0, 0, 1]));
        pool.select(&context).unwrap().upstream.address().to_owned()
    }

    #[test]
    fn breaker_opens_on_error_ratio_and_recovers_through_half_open() {
        let (pool, first) = breaker_pool(breaker(Some(0.5), None));
        let fast = Duration::ZERO;

        // Three errors out of three: under `min_requests`, still closed.
        first.record_failure();
        first.record_response(503, fast);
        first.record_response(500, fast);
        assert_eq!(first.circuit(), Some(CircuitState::Closed));
        assert_eq!(selected(&pool), "10.0.0.1:80");

        first.record_response(200, fast);
        assert_eq!(first.circuit(), Some(CircuitState::Open));
        assert!(!first.is_healthy());
        assert_eq!(selected(&pool), "10.0.0.2:80");
        // Results of requests already in flight do not close it.
        first.record_response(200, fast);
        assert_eq!(first.circuit(), Some(CircuitState::Open));

        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(first.circuit(), Some(CircuitState::HalfOpen));
        assert_eq!(selected(&pool), "10.0.0.1:80");
        // One trial at a time.
        let trial = first.start_request();
        assert_eq!(selected(&pool), "10.0.0.2:80");

        // A failed trial reopens the breaker at once.
        first.record_response(502, fast);
        drop(trial);
        assert_eq!(first.circuit(), Some(CircuitState::Open));

        std::thread::sleep(Duration::from_millis(60));
        first.record_response(404, fast);
        assert_eq!(first.circuit(), Some(CircuitState::Closed));
        // The window starts again: old errors are forgotten.
        first.record_response(500, fast);
        first.record_response(200, fast);
        first.record_response(200, fast);
        first.record_response(200, fast);
        assert_eq!(first.circuit(), Some(CircuitState::Closed));
        assert_eq!(selected(&pool), "10.0.0.1:80");
    }

    #[test]
    fn breaker_stays_closed_below_the_ratio_and_forgets_old_windows() {
        let mut settings = breaker(Some(0.5), None);
        settings.window = Duration::from_millis(40);
        let (_pool, first) = breaker_pool(settings);

        for status in [500, 200, 200, 200, 200, 200] {
            first.record_response(status, Duration::ZERO);
        }
        assert_eq!(first.circuit(), Some(CircuitState::Closed));

        first.record_failure();
        first.record_failure();
        first.record_failure();
        std::thread::sleep(Duration::from_millis(60));
        // The fourth error falls in a new window.
        first.record_failure();
        assert_eq!(first.circuit(), Some(CircuitState::Closed));
    }

    #[test]
    fn breaker_opens_on_latency() {
        let limit = Duration::from_millis(100);
        let (_pool, first) = breaker_pool(breaker(None, Some(limit)));

        // Errors alone do not open a latency breaker.
        for _ in 0..4 {
            first.record_response(500, limit);
        }
        assert_eq!(first.circuit(), Some(CircuitState::Closed));

        let slow = Duration::from_millis(101);
        for latency in [slow, slow, slow, slow] {
            first.record_response(200, latency);
        }
        assert_eq!(first.circuit(), Some(CircuitState::Open));

        // A slow trial reopens it.
        std::thread::sleep(Duration::from_millis(60));
        first.record_response(200, slow);
        assert_eq!(first.circuit(), Some(CircuitState::Open));
    }

    #[test]
    fn breaker_counts_connections_and_is_absent_without_settings() {
        let (_pool, first) = breaker_pool(breaker(Some(0.5), None));
        first.record_success();
        first.record_success();
        first.record_success();
        first.record_failure();
        assert_eq!(first.circuit(), Some(CircuitState::Closed));
        first.record_failure();
        first.record_failure();
        assert_eq!(first.circuit(), Some(CircuitState::Open));

        let pool = Pool::new("p", &group(Policy::First, &[("10.0.0.1:80", 1)])).unwrap();
        let plain = &pool.upstreams()[0];
        for _ in 0..20 {
            plain.record_failure();
        }
        assert_eq!(plain.circuit(), None);
        assert!(plain.is_healthy());
    }

    fn slow_start_pool(policy: Policy, slow_start: Duration) -> Pool {
        let mut group = group(policy, &[("10.0.0.1:80", 1), ("10.0.0.2:80", 1)]);
        group.slow_start = Some(slow_start);
        group.passive = Some(PassiveHealth {
            fail_duration: Duration::from_millis(40),
            max_fails: 1,
            unhealthy_status: Vec::new(),
            unhealthy_latency: None,
            unhealthy_request_count: 0,
        });
        Pool::new("p", &group).unwrap()
    }

    /// Requests out of 1000 that go to `address`.
    fn picks_of(pool: &Pool, address: &str) -> usize {
        (0..1000).filter(|_| selected(pool) == address).count()
    }

    #[test]
    fn slow_start_ramps_up_added_backends() {
        let long = Duration::from_secs(3600);
        for policy in [
            Policy::RoundRobin,
            Policy::WeightedRoundRobin,
            Policy::Random,
            Policy::RandomChoose,
            Policy::LeastConn,
            Policy::First,
        ] {
            let pool = slow_start_pool(policy, long);
            // Members at startup take their full share at once.
            for upstream in pool.upstreams() {
                assert!(upstream.ramp_admits());
            }

            // Busy old members, so load aware policies would favour the new one.
            let _busy: Vec<RequestGuard> = pool
                .upstreams()
                .iter()
                .map(Upstream::start_request)
                .collect();
            pool.add(Backend::new("10.0.0.3:80")).unwrap();
            assert_eq!(picks_of(&pool, "10.0.0.3:80"), 0, "{policy:?}");

            // Without slow start the new member gets traffic at once.
            let plain = Pool::new("p", &group(policy, &[("10.0.0.1:80", 1)])).unwrap();
            let _busy = plain.upstreams()[0].start_request();
            plain.add(Backend::new("10.0.0.3:80")).unwrap();
            if policy != Policy::First {
                assert!(picks_of(&plain, "10.0.0.3:80") > 100, "{policy:?}");
            }
        }

        // Members found by DNS ramp up as well.
        let pool = slow_start_pool(Policy::RoundRobin, long);
        pool.sync_dynamic(vec![Backend::new("10.0.1.1:80")]);
        assert_eq!(picks_of(&pool, "10.0.1.1:80"), 0);
    }

    #[test]
    fn slow_start_share_grows_with_time_and_ends() {
        let pool = slow_start_pool(Policy::RoundRobin, Duration::from_secs(10));
        pool.add(Backend::new("10.0.0.3:80")).unwrap();
        let added = Arc::clone(&pool.upstreams()[2]);
        let ramp = added.ramp.as_ref().unwrap();

        // Half way: half of its round robin share of 1000 / 3.
        *ramp.started.lock().unwrap() = Instant::now() - Duration::from_secs(5);
        let picks = picks_of(&pool, "10.0.0.3:80");
        assert!((100..240).contains(&picks), "{picks}");

        *ramp.started.lock().unwrap() = Instant::now() - Duration::from_secs(10);
        assert!((330..=334).contains(&picks_of(&pool, "10.0.0.3:80")));
        assert!(!ramp.ramping.load(Ordering::Relaxed));
    }

    #[test]
    fn slow_start_still_serves_when_only_ramping_members_are_left() {
        let pool = slow_start_pool(Policy::RoundRobin, Duration::from_secs(3600));
        pool.remove("10.0.0.1:80");
        pool.remove("10.0.0.2:80");
        pool.add(Backend::new("10.0.0.3:80")).unwrap();
        assert_eq!(picks_of(&pool, "10.0.0.3:80"), 1000);
    }

    #[test]
    fn slow_start_ramps_up_recovered_backends() {
        let long = Duration::from_secs(3600);

        // Active health checks.
        let pool = slow_start_pool(Policy::RoundRobin, long);
        let second = Arc::clone(&pool.upstreams()[1]);
        assert_eq!(second.record_check(false, 1, 1), Some(false));
        assert_eq!(picks_of(&pool, "10.0.0.2:80"), 0);
        assert_eq!(second.record_check(true, 1, 1), Some(true));
        assert!(second.is_available());
        assert_eq!(picks_of(&pool, "10.0.0.2:80"), 0);

        // Passive health checks, once the failure expires.
        let pool = slow_start_pool(Policy::RoundRobin, long);
        let second = Arc::clone(&pool.upstreams()[1]);
        second.record_failure();
        assert!(!second.is_healthy());
        std::thread::sleep(Duration::from_millis(60));
        assert!(second.is_healthy());
        assert_eq!(picks_of(&pool, "10.0.0.2:80"), 0);

        // Circuit breaker, after a good trial request.
        let mut group = group(
            Policy::RoundRobin,
            &[("10.0.0.1:80", 1), ("10.0.0.2:80", 1)],
        );
        group.slow_start = Some(long);
        group.circuit_breaker = Some(breaker(Some(0.5), None));
        let pool = Pool::new("p", &group).unwrap();
        let second = Arc::clone(&pool.upstreams()[1]);
        for _ in 0..4 {
            second.record_failure();
        }
        assert_eq!(second.circuit(), Some(CircuitState::Open));
        std::thread::sleep(Duration::from_millis(60));
        // The half open trial is not held back by slow start.
        assert!(picks_of(&pool, "10.0.0.2:80") > 0);
        second.record_response(200, Duration::ZERO);
        assert_eq!(second.circuit(), Some(CircuitState::Closed));
        assert_eq!(picks_of(&pool, "10.0.0.2:80"), 0);
    }
}
