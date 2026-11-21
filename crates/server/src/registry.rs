//! Live load balancing groups of one configuration generation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use regex::Regex;
use rxh_core::config::{
    Action, Config, ConfigError, Forward, Protocol, Server as ServerConfig, UpstreamGroup,
};
use rxh_core::pool::{self, Pool};
use tokio::task::JoinSet;

use crate::admin_guard::AdminGuard;
use crate::dynamic;
use crate::error::Error;
use crate::health;
use crate::http::transport::Transport;
use crate::per_core;
use crate::stream::Dialer;

/// A pool plus the connections and checks attached to it.
#[derive(Debug)]
pub struct Group {
    /// Members, health and selection.
    pub pool: Pool,

    /// Opens connections to the members.
    pub(crate) dialer: Arc<Dialer>,

    /// Pooled upstream HTTP connections.
    pub(crate) transport: Transport,

    /// Compiled `health.body` expression.
    pub(crate) health_body: Option<Regex>,
}

impl Group {
    /// `runtimes` is the number of runtimes that keep their own upstream
    /// connections.
    fn new(pool: Pool, runtimes: usize, guard: AdminGuard) -> Result<Self, Error> {
        let body = pool
            .settings()
            .health
            .as_ref()
            .and_then(|health| health.body.as_deref());
        let health_body =
            match body {
                Some(body) => Some(Regex::new(body).map_err(|err| {
                    invalid(format!("upstream {}: health.body: {err}", pool.name()))
                })?),
                None => None,
            };
        let dialer = Dialer::new(pool.settings(), guard)
            .map(Arc::new)
            .map_err(|err| invalid(format!("upstream {}: tls: {err}", pool.name())))?;
        let transport = Transport::new(pool.settings(), Arc::clone(&dialer), runtimes);
        Ok(Self {
            pool,
            dialer,
            transport,
            health_body,
        })
    }
}

/// Every pool of a configuration, named and inline.
#[derive(Debug, Default)]
pub struct Registry {
    groups: BTreeMap<String, Arc<Group>>,
    guard: AdminGuard,
}

impl Registry {
    /// Builds all pools described by `config`. Inline `forward` groups get
    /// names like `server[0]#1`, see [`Registry::inline_name`].
    pub fn build(config: &Config) -> Result<Self, Error> {
        let mut groups = BTreeMap::new();
        let runtimes = per_core::count(config.runtime);
        let guard = AdminGuard::new(&config.admin);

        // Pools that serve UDP servers cannot have unix socket members.
        let mut udp_pools = BTreeSet::new();
        for (server_index, server) in config.servers.iter().enumerate() {
            if server.protocol != Protocol::Udp {
                continue;
            }
            for (pattern_index, pattern) in &server.entries() {
                if let Action::Forward(forward) = &pattern.action {
                    match forward {
                        Forward::Group(name) => udp_pools.insert(name.clone()),
                        Forward::Inline(_) => {
                            udp_pools.insert(Self::inline_name(server_index, server, pattern_index))
                        }
                        // Validation rejects splits on UDP servers.
                        Forward::Split(_) => false,
                    };
                }
            }
        }
        let pool_for = |name: &String, group: &UpstreamGroup| -> Result<Pool, Error> {
            let pool =
                Pool::new(name.clone(), group).map_err(|err| invalid(format!("{name}: {err}")))?;
            Ok(if udp_pools.contains(name) {
                pool.without_unix()
            } else {
                pool
            })
        };

        for (name, group) in &config.upstreams {
            let pool = pool_for(name, group)?;
            groups.insert(
                name.clone(),
                Arc::new(Group::new(pool, runtimes, guard.clone())?),
            );
        }

        for (server_index, server) in config.servers.iter().enumerate() {
            for (pattern_index, pattern) in &server.entries() {
                if let Action::Forward(Forward::Inline(group)) = &pattern.action {
                    let name = Self::inline_name(server_index, server, pattern_index);
                    let pool = pool_for(&name, group)?;
                    groups.insert(name, Arc::new(Group::new(pool, runtimes, guard.clone())?));
                }
            }
        }

        Ok(Self { groups, guard })
    }

    /// Name given to the inline group of a pattern.
    #[must_use]
    pub fn inline_name(
        server_index: usize,
        server: &ServerConfig,
        pattern_index: impl Display,
    ) -> String {
        match &server.name {
            Some(name) => format!("{name}#{pattern_index}"),
            None => format!("server[{server_index}]#{pattern_index}"),
        }
    }

    /// Groups referenced by a pattern's `forward`.
    pub(crate) fn resolve(
        &self,
        forward: &Forward,
        server_index: usize,
        server: &ServerConfig,
        pattern_index: impl Display,
    ) -> Result<Split, Error> {
        let lookup = |name: &str| {
            self.get(name)
                .ok_or_else(|| invalid(format!("unknown upstream group @{name}")))
        };
        match forward {
            Forward::Group(name) => Ok(Split::single(lookup(name)?)),
            Forward::Inline(_) => Ok(Split::single(lookup(&Self::inline_name(
                server_index,
                server,
                pattern_index,
            ))?)),
            Forward::Split(targets) => {
                let mut groups = Vec::with_capacity(targets.len());
                for target in targets {
                    groups.push(lookup(&target.pool)?);
                }
                let weights: Vec<u32> = targets.iter().map(|target| target.weight).collect();
                Ok(Split {
                    groups,
                    order: pool::smooth_weighted_order(&weights),
                    cursor: AtomicUsize::new(0),
                })
            }
        }
    }

    /// What the connections of this configuration keep away from, for the
    /// dialers outside the pools.
    pub(crate) fn guard(&self) -> &AdminGuard {
        &self.guard
    }

    /// Looks up a group by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<Group>> {
        self.groups.get(name).cloned()
    }

    /// All groups, sorted by name.
    pub fn groups(&self) -> impl Iterator<Item = &Arc<Group>> {
        self.groups.values()
    }

    /// Seeds the DNS driven pools with the members of the same pools in
    /// `previous` and keeps the drain and maintenance states of the members,
    /// see [`Pool::inherit_dynamic`] and [`Pool::inherit_states`].
    pub(crate) fn inherit(&self, previous: &Registry) {
        for (name, group) in &self.groups {
            if let Some(old) = previous.groups.get(name) {
                group.pool.inherit_dynamic(&old.pool);
                group.pool.inherit_states(&old.pool);
            }
        }
    }

    /// Starts the active health checks and the DNS refreshers of the groups
    /// that configure them. Dropping the returned set stops them all.
    #[must_use]
    pub fn start_background_tasks(&self) -> JoinSet<()> {
        let mut tasks = JoinSet::new();
        for group in self.groups.values() {
            if group.pool.settings().health.is_some() {
                tasks.spawn(health::run(Arc::clone(group)));
            }
            if group.pool.settings().dynamic.is_some() {
                tasks.spawn(dynamic::run(Arc::clone(group)));
            }
        }
        tasks
    }
}

/// The groups of one `forward` and how traffic is split between them.
#[derive(Debug)]
pub(crate) struct Split {
    groups: Vec<Arc<Group>>,
    /// Smooth weighted round robin cycle, as indexes into `groups`.
    order: Vec<usize>,
    cursor: AtomicUsize,
}

impl Split {
    fn single(group: Arc<Group>) -> Self {
        Self {
            groups: vec![group],
            order: vec![0],
            cursor: AtomicUsize::new(0),
        }
    }

    /// Group for the next request or connection. The weighted turn goes to
    /// the next group of the cycle with an available member, so a canary
    /// whose members are all down sends its share to the other pools; when
    /// none has one, the turn's group answers (with its usual error).
    pub(crate) fn pick(&self) -> &Arc<Group> {
        if let [group] = self.groups.as_slice() {
            return group;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let turn = |offset: usize| &self.groups[self.order[(start + offset) % self.order.len()]];
        (0..self.order.len())
            .map(turn)
            .find(|group| group.pool.is_available())
            .unwrap_or_else(|| turn(0))
    }
}

fn invalid(message: String) -> Error {
    Error::Config(ConfigError::Invalid(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilt_registry_inherits_dynamic_members_by_name() {
        let config = Config::from_toml(
            "[upstream.dns]\nbackends = []\ndynamic = { a = \"x.test:80\" }\n\
             [upstream.other]\nbackends = []\ndynamic = { a = \"y.test:80\" }\n",
        )
        .unwrap();
        let old = Registry::build(&config).unwrap();
        old.get("dns")
            .unwrap()
            .pool
            .sync_dynamic(vec![rxh_core::config::Backend::new("10.0.0.1:80")]);

        let new = Registry::build(&config).unwrap();
        new.inherit(&old);
        assert_eq!(new.get("dns").unwrap().pool.upstreams().len(), 1);
        assert!(new.get("other").unwrap().pool.upstreams().is_empty());
    }
}
