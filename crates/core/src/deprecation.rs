//! Deprecated configuration keys, found in the text of a file.
//!
//! Deprecated spellings keep working (they are serde aliases), so the parsed
//! [`Config`](crate::config::Config) cannot tell them apart. This module
//! walks the raw TOML instead, at the places where each spelling is read.
//! `docs/migration.md` lists every entry of [`DEPRECATED`].

use std::fmt;

use toml::{Table, Value};

/// Where a deprecated spelling is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A key of a `[[server]]` table.
    Server,
    /// A key of a pool: `[upstream.NAME]` or an inline `forward` table.
    Pool,
    /// A value of a pool's `policy` (or its aliases).
    Policy,
}

/// One deprecated spelling and what replaces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deprecated {
    pub scope: Scope,
    pub old: &'static str,
    pub new: &'static str,
    /// Release that deprecated it. Removal only comes with a major release.
    pub since: &'static str,
}

/// Every deprecated spelling, oldest first.
pub const DEPRECATED: &[Deprecated] = &[
    Deprecated {
        scope: Scope::Server,
        old: "connections",
        new: "max_connections",
        since: "0.3.0",
    },
    Deprecated {
        scope: Scope::Pool,
        old: "algorithm",
        new: "policy",
        since: "0.3.0",
    },
    Deprecated {
        scope: Scope::Policy,
        old: "WRR",
        new: "weighted_round_robin",
        since: "0.3.0",
    },
    Deprecated {
        scope: Scope::Policy,
        old: "wrr",
        new: "weighted_round_robin",
        since: "0.3.0",
    },
];

/// Keys that hold a pool's policy, the deprecated one included.
const POLICY_KEYS: [&str; 3] = ["policy", "lb_policy", "algorithm"];

/// A deprecated spelling found in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// Dotted path of the table that holds it, like `server[0].match[1].forward`.
    pub path: String,
    pub deprecated: Deprecated,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Deprecated {
            scope,
            old,
            new,
            since,
        } = self.deprecated;
        let what = match scope {
            Scope::Server | Scope::Pool => "key",
            Scope::Policy => "policy",
        };
        write!(
            f,
            "{}: {what} `{old}` is deprecated since {since}, use `{new}`",
            self.path
        )
    }
}

/// Deprecated spellings in the text of a main or included configuration
/// file, in file order. A file that is not valid TOML has none: the parser
/// reports it.
#[must_use]
pub fn scan(text: &str) -> Vec<Warning> {
    let Ok(root) = text.parse::<Table>() else {
        return Vec::new();
    };
    let mut warnings: Vec<Warning> = Vec::new();
    if let Some(Value::Table(pools)) = root.get("upstream") {
        for (name, pool) in pools {
            if let Value::Table(pool) = pool {
                check_pool(&format!("upstream.{name}"), pool, &mut warnings);
            }
        }
    }
    if let Some(Value::Array(servers)) = root.get("server") {
        for (index, server) in servers.iter().enumerate() {
            if let Value::Table(server) = server {
                check_server(&format!("server[{index}]"), server, &mut warnings);
            }
        }
    }
    warnings
}

fn check_server(path: &str, server: &Table, warnings: &mut Vec<Warning>) {
    check_keys(path, server, Scope::Server, warnings);
    if let Some(Value::Table(pool)) = server.get("forward") {
        check_pool(&format!("{path}.forward"), pool, warnings);
    }
    if let Some(Value::Array(patterns)) = server.get("match") {
        for (index, pattern) in patterns.iter().enumerate() {
            if let Some(Value::Table(pool)) = pattern.get("forward") {
                check_pool(&format!("{path}.match[{index}].forward"), pool, warnings);
            }
        }
    }
}

fn check_pool(path: &str, pool: &Table, warnings: &mut Vec<Warning>) {
    check_keys(path, pool, Scope::Pool, warnings);
    for key in POLICY_KEYS {
        let Some(Value::String(policy)) = pool.get(key) else {
            continue;
        };
        for deprecated in DEPRECATED {
            if deprecated.scope == Scope::Policy && deprecated.old == policy {
                warnings.push(Warning {
                    path: path.to_owned(),
                    deprecated: *deprecated,
                });
            }
        }
    }
}

fn check_keys(path: &str, table: &Table, scope: Scope, warnings: &mut Vec<Warning>) {
    for deprecated in DEPRECATED {
        if deprecated.scope == scope && table.contains_key(deprecated.old) {
            warnings.push(Warning {
                path: path.to_owned(),
                deprecated: *deprecated,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, IncludedConfig, Policy};

    fn messages(text: &str) -> Vec<String> {
        scan(text).iter().map(ToString::to_string).collect()
    }

    const OLD: &str = r#"
[upstream.web]
algorithm = "WRR"
backends = ["127.0.0.1:8001"]

[upstream.api]
lb_policy = "wrr"
to = ["127.0.0.1:8002"]

[[server]]
listen = "127.0.0.1:8080"
connections = 10
forward = { algorithm = "round_robin", backends = ["127.0.0.1:8003"] }

[[server]]
listen = "127.0.0.1:8081"
match = [{ uri = "/a", forward = { policy = "wrr", backends = ["127.0.0.1:8004"] } }]
"#;

    #[test]
    fn finds_every_deprecated_spelling_with_its_path() {
        assert_eq!(
            messages(OLD),
            [
                "upstream.api: policy `wrr` is deprecated since 0.3.0, use `weighted_round_robin`",
                "upstream.web: key `algorithm` is deprecated since 0.3.0, use `policy`",
                "upstream.web: policy `WRR` is deprecated since 0.3.0, use `weighted_round_robin`",
                "server[0]: key `connections` is deprecated since 0.3.0, use `max_connections`",
                "server[0].forward: key `algorithm` is deprecated since 0.3.0, use `policy`",
                "server[1].match[0].forward: policy `wrr` is deprecated since 0.3.0, \
                 use `weighted_round_robin`",
            ]
        );
    }

    #[test]
    fn deprecated_spellings_still_parse_like_their_replacement() {
        let config = Config::from_toml(OLD).expect("deprecated keys keep working");
        assert_eq!(config.servers[0].max_connections, 10);
        assert_eq!(config.upstreams["web"].policy, Policy::WeightedRoundRobin);
        assert_eq!(config.upstreams["api"].policy, Policy::WeightedRoundRobin);
        IncludedConfig::from_toml(OLD).expect("included files accept them too");
    }

    #[test]
    fn current_keys_and_caddy_aliases_are_not_deprecated() {
        let text = r#"
[upstream.connections]
policy = "weighted_round_robin"
lb_retries = 2
to = ["127.0.0.1:8001"]

[upstream.algorithm]
lb_policy = "round_robin"
backends = ["127.0.0.1:8002"]

[[server]]
listen = "127.0.0.1:8080"
max_connections = 10
request = { headers = { set = { connections = "kept" } } }
forward = "@connections"
"#;
        Config::from_toml(text).expect("valid");
        assert_eq!(messages(text), Vec::<String>::new());
    }

    #[test]
    fn invalid_toml_has_no_warnings() {
        assert!(scan("[[server]\nconnections = 1").is_empty());
    }
}
