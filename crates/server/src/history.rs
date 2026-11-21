//! One hour of statistics kept in memory for the charts of the web UI.
//!
//! A thread takes a sample every [`INTERVAL`] and keeps the last
//! [`CAPACITY`]. Like the metrics, the history is process wide: it outlives
//! configuration reloads and the runtimes that serve the admin API.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Once, OnceLock, PoisonError, Weak};
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Serialize;

use crate::admin::{self, AdminState};
use crate::metrics::{self, ServerCounters};
use crate::registry::Registry;

/// Time between two samples.
pub(crate) const INTERVAL: Duration = Duration::from_secs(5);

/// Samples kept: one hour.
pub(crate) const CAPACITY: usize = 720;

/// Counters of one pool, summed over its backends.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub(crate) struct PoolCounters {
    requests: u64,
    failures: u64,
    responses: u64,
    server_errors: u64,
    response_seconds: f64,
    active: u64,
    healthy: u32,
    backends: u32,
}

/// Every counter at one point in time.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Sample {
    /// Unix time in seconds.
    time: u64,
    servers: BTreeMap<String, ServerCounters>,
    pools: BTreeMap<String, PoolCounters>,
}

impl Sample {
    /// Leaves the series of `servers` only, and no pool: what a caller
    /// without the `admin` role gets.
    pub(crate) fn keep_servers(&mut self, servers: &BTreeSet<String>) {
        self.servers.retain(|server, _| servers.contains(server));
        self.pools.clear();
    }
}

/// The last [`CAPACITY`] samples, oldest first.
#[derive(Default)]
pub(crate) struct History {
    samples: Mutex<VecDeque<Sample>>,
    /// Admin state of the daemon that started the sampler last; its
    /// registry is read at each tick, so the pools follow reloads.
    source: Mutex<Weak<AdminState>>,
    started: Once,
}

/// The history of this process.
pub(crate) fn global() -> &'static History {
    static HISTORY: OnceLock<History> = OnceLock::new();
    HISTORY.get_or_init(History::default)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl History {
    /// Adds a sample, dropping the oldest beyond [`CAPACITY`].
    pub(crate) fn push(&self, sample: Sample) {
        let mut samples = lock(&self.samples);
        if samples.len() == CAPACITY {
            samples.pop_front();
        }
        samples.push_back(sample);
    }

    /// Copy of the samples taken after the unix time `since` (all of them
    /// without it), oldest first.
    pub(crate) fn samples(&self, since: Option<u64>) -> Vec<Sample> {
        let samples = lock(&self.samples);
        samples
            .iter()
            .filter(|sample| since.is_none_or(|since| sample.time > since))
            .cloned()
            .collect()
    }

    /// Samples the pools of `state` from now on. The first call takes a
    /// sample at once and starts the sampler thread; later calls, from
    /// another runtime, a rebound admin listener or another daemon, only
    /// switch the state it reads.
    pub(crate) fn start(&'static self, state: &Arc<AdminState>) {
        *lock(&self.source) = Arc::downgrade(state);
        self.started.call_once(|| {
            self.tick();
            // A thread rather than a task: it must outlive the runtime that
            // started it.
            let sampler = thread::Builder::new()
                .name(String::from("rxh-history"))
                .spawn(move || {
                    loop {
                        thread::sleep(INTERVAL);
                        self.tick();
                    }
                });
            if let Err(err) = sampler {
                eprintln!("Admin => Statistics history not recorded: {err}");
            }
        });
    }

    /// Pushes a sample. Without a live source, the pools are left out but
    /// the server counters, process wide, are still sampled.
    fn tick(&self) {
        let state = lock(&self.source).upgrade();
        let sample = match state {
            Some(state) => sample(&admin::registry(&state)),
            None => Sample {
                time: unix_time(),
                servers: metrics::global().snapshot(),
                pools: BTreeMap::new(),
            },
        };
        self.push(sample);
    }
}

/// Counters of every server and every pool of `registry`, now.
pub(crate) fn sample(registry: &Registry) -> Sample {
    let mut pools: BTreeMap<String, PoolCounters> = BTreeMap::new();
    for group in registry.groups() {
        let mut counters = PoolCounters::default();
        for upstream in group.pool.upstreams() {
            counters.requests += upstream.total_requests();
            counters.failures += upstream.total_failures();
            counters.responses += upstream.total_responses();
            counters.server_errors += upstream.total_server_errors();
            counters.response_seconds += upstream.total_response_time().as_secs_f64();
            counters.active += upstream.active_requests() as u64;
            counters.healthy += u32::from(upstream.is_healthy());
            counters.backends += 1;
        }
        pools.insert(group.pool.name().to_owned(), counters);
    }
    Sample {
        time: unix_time(),
        servers: metrics::global().snapshot(),
        pools,
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use rxh_core::config::{AdminListen, Config};
    use tokio::sync::mpsc;

    use crate::admin::Current;

    fn empty_sample(time: u64) -> Sample {
        Sample {
            time,
            servers: BTreeMap::new(),
            pools: BTreeMap::new(),
        }
    }

    #[test]
    fn the_ring_keeps_the_last_720() {
        let history = History::default();
        for time in 0..725 {
            history.push(empty_sample(time));
        }
        let samples = history.samples(None);
        assert_eq!(samples.len(), 720);
        assert_eq!(samples[0].time, 5);
        assert_eq!(samples[719].time, 724);

        let times = |since: u64| -> Vec<u64> {
            let samples = history.samples(Some(since));
            samples.iter().map(|sample| sample.time).collect()
        };
        assert_eq!(times(721), [722, 723, 724]);
        assert_eq!(times(724), [] as [u64; 0]);
        assert_eq!(times(0).len(), 720);
    }

    #[test]
    fn a_sample_sums_the_backends_of_a_pool() {
        let config =
            Config::from_toml("[upstream.api]\nbackends = [\"127.0.0.1:1\", \"127.0.0.1:2\"]\n")
                .unwrap();
        let registry = Registry::build(&config).unwrap();
        let upstreams = registry.get("api").unwrap().pool.upstreams();
        upstreams[1].record_check(
            /* passed= */ false, /* passes= */ 1, /* fails= */ 1,
        );
        let _request = upstreams[0].start_request();

        let pool = &sample(&registry).pools["api"];
        assert_eq!((pool.healthy, pool.backends), (1, 2));
        assert_eq!((pool.requests, pool.active), (1, 1));
    }

    fn admin_state() -> Arc<AdminState> {
        let config = Config::default();
        Arc::new(AdminState {
            started: Instant::now(),
            config_path: None,
            listen: AdminListen::Tcp("127.0.0.1:0".parse().unwrap()),
            token: None,
            tls: None,
            current: std::sync::RwLock::new(Current {
                registry: Arc::new(Registry::build(&config).unwrap()),
                config: Arc::new(config),
                servers: Vec::new(),
            }),
            control: mpsc::channel(1).0,
            saving: tokio::sync::Mutex::new(()),
            hashing: admin::hashing_permits(),
            access: crate::access::Store::open(std::path::PathBuf::new()),
            sessions: crate::session::Sessions::new(),
            oidc: None,
        })
    }

    #[test]
    fn a_second_start_does_not_start_a_second_sampler() {
        let state = admin_state();
        let history: &'static History = Box::leak(Box::default());
        history.start(&state);
        history.start(&state);
        assert_eq!(history.samples(None).len(), 1);
    }

    #[test]
    fn a_dead_source_still_samples_the_servers() {
        metrics::global().record_http("history-dead-source", 200, Duration::from_millis(1));
        let history = History::default();
        let state = admin_state();
        *lock(&history.source) = Arc::downgrade(&state);
        drop(state);
        history.tick();
        let samples = history.samples(None);
        assert_eq!(samples.len(), 1);
        assert!(samples[0].pools.is_empty());
        assert_eq!(samples[0].servers["history-dead-source"].status[1], 1);
    }
}
