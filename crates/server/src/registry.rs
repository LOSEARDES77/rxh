//! Live load balancing groups of one configuration generation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use regex::Regex;
use rxh_core::config::{
    Action, Config, ConfigError, Forward, Protocol, Server as ServerConfig, UpstreamGroup,
};
use rxh_core::pool::Pool;
use tokio::task::JoinSet;

use crate::dynamic;
use crate::error::Error;
use crate::health;
use crate::http::transport::Transport;
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
    fn new(pool: Pool) -> Result<Self, Error> {
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
        let dialer = Dialer::new(pool.settings())
            .map(Arc::new)
            .map_err(|err| invalid(format!("upstream {}: tls: {err}", pool.name())))?;
        let transport = Transport::new(pool.settings(), Arc::clone(&dialer));
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
}

impl Registry {
    /// Builds all pools described by `config`. Inline `forward` groups get
    /// names like `server[0]#1`, see [`Registry::inline_name`].
    pub fn build(config: &Config) -> Result<Self, Error> {
        let mut groups = BTreeMap::new();

        // Pools that serve UDP servers cannot have unix socket members.
        let mut udp_pools = BTreeSet::new();
        for (server_index, server) in config.servers.iter().enumerate() {
            if server.protocol != Protocol::Udp {
                continue;
            }
            for (pattern_index, pattern) in server.patterns.iter().enumerate() {
                if let Action::Forward(forward) = &pattern.action {
                    udp_pools.insert(match forward {
                        Forward::Group(name) => name.clone(),
                        Forward::Inline(_) => {
                            Self::inline_name(server_index, server, pattern_index)
                        }
                    });
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
            groups.insert(name.clone(), Arc::new(Group::new(pool)?));
        }

        for (server_index, server) in config.servers.iter().enumerate() {
            for (pattern_index, pattern) in server.patterns.iter().enumerate() {
                if let Action::Forward(Forward::Inline(group)) = &pattern.action {
                    let name = Self::inline_name(server_index, server, pattern_index);
                    let pool = pool_for(&name, group)?;
                    groups.insert(name, Arc::new(Group::new(pool)?));
                }
            }
        }

        Ok(Self { groups })
    }

    /// Name given to the inline group of a pattern.
    #[must_use]
    pub fn inline_name(server_index: usize, server: &ServerConfig, pattern_index: usize) -> String {
        match &server.name {
            Some(name) => format!("{name}#{pattern_index}"),
            None => format!("server[{server_index}]#{pattern_index}"),
        }
    }

    /// Group referenced by a pattern's `forward`.
    pub(crate) fn resolve(
        &self,
        forward: &Forward,
        server_index: usize,
        server: &ServerConfig,
        pattern_index: usize,
    ) -> Result<Arc<Group>, Error> {
        let name = match forward {
            Forward::Group(name) => name.clone(),
            Forward::Inline(_) => Self::inline_name(server_index, server, pattern_index),
        };
        self.get(&name)
            .ok_or_else(|| invalid(format!("unknown upstream group @{name}")))
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
    /// `previous`, see [`Pool::inherit_dynamic`].
    pub(crate) fn inherit_dynamic(&self, previous: &Registry) {
        for (name, group) in &self.groups {
            if let Some(old) = previous.groups.get(name) {
                group.pool.inherit_dynamic(&old.pool);
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
        new.inherit_dynamic(&old);
        assert_eq!(new.get("dns").unwrap().pool.upstreams().len(), 1);
        assert!(new.get("other").unwrap().pool.upstreams().is_empty());
    }
}
