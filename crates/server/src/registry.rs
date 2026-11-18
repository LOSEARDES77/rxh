//! Live load balancing groups of one configuration generation.

use std::collections::BTreeMap;
use std::sync::Arc;

use regex::Regex;
use rxh_core::config::{Action, Config, ConfigError, Forward, Server as ServerConfig};
use rxh_core::pool::Pool;
use tokio::task::JoinSet;

use crate::error::Error;
use crate::health;
use crate::http::transport::Transport;

/// A pool plus the connections and checks attached to it.
#[derive(Debug)]
pub struct Group {
    /// Members, health and selection.
    pub pool: Pool,

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
        let transport = Transport::new(pool.settings());
        Ok(Self {
            pool,
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

        for (name, group) in &config.upstreams {
            let pool =
                Pool::new(name.clone(), group).map_err(|err| invalid(format!("{name}: {err}")))?;
            groups.insert(name.clone(), Arc::new(Group::new(pool)?));
        }

        for (server_index, server) in config.servers.iter().enumerate() {
            for (pattern_index, pattern) in server.patterns.iter().enumerate() {
                if let Action::Forward(Forward::Inline(group)) = &pattern.action {
                    let name = Self::inline_name(server_index, server, pattern_index);
                    let pool = Pool::new(name.clone(), group)
                        .map_err(|err| invalid(format!("{name}: {err}")))?;
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

    /// Starts active health checks for every group that configures them.
    /// Dropping the returned set stops the checks.
    #[must_use]
    pub fn start_health_checks(&self) -> JoinSet<()> {
        let mut tasks = JoinSet::new();
        for group in self.groups.values() {
            if group.pool.settings().health.is_some() {
                tasks.spawn(health::run(Arc::clone(group)));
            }
        }
        tasks
    }
}

fn invalid(message: String) -> Error {
    Error::Config(ConfigError::Invalid(message))
}
