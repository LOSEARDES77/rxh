//! Edits the configuration file in place, keeping comments and formatting.
//! Every change is validated as a whole before it is written.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use rxh_core::config::{Config, Policy};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value, value};

/// New pool definition.
#[derive(Debug, Default)]
pub(crate) struct PoolSpec {
    pub(crate) policy: String,
    pub(crate) backends: Vec<String>,
    pub(crate) key: Option<String>,
    pub(crate) retries: Option<u32>,
    pub(crate) health_uri: Option<String>,
    pub(crate) health_interval: Option<String>,
    pub(crate) fail_duration: Option<String>,
}

/// New service definition.
#[derive(Debug, Default)]
pub(crate) struct ServiceSpec {
    pub(crate) listen: Vec<SocketAddr>,
    pub(crate) protocol: &'static str,
    pub(crate) to: Vec<String>,
    pub(crate) serve: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) uri: Option<String>,
}

/// A configuration file loaded for editing.
pub(crate) struct ConfigFile {
    path: PathBuf,
    document: DocumentMut,
}

impl ConfigFile {
    /// Loads `path`. A missing file starts as an empty document.
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let document = text
            .parse()
            .map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            document,
        })
    }

    #[cfg(test)]
    fn from_text(text: &str) -> Self {
        Self {
            path: PathBuf::from("test.toml"),
            document: text.parse().expect("valid TOML"),
        }
    }

    /// Path of the file.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn pool_mut(&mut self, pool: &str) -> Result<&mut Table, String> {
        self.document
            .get_mut("upstream")
            .and_then(|upstreams| upstreams.get_mut(pool))
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                format!(
                    "pool {pool} is not defined in {}; create it with `rxh pool add {pool}`",
                    self.path.display()
                )
            })
    }

    /// Appends a backend to `[upstream.POOL].backends`.
    pub(crate) fn add_backend(
        &mut self,
        pool: &str,
        address: &str,
        weight: u32,
    ) -> Result<(), String> {
        let table = self.pool_mut(pool)?;
        let key = if table.contains_key("to") {
            "to"
        } else {
            "backends"
        };
        let backends = table
            .entry(key)
            .or_insert_with(|| value(Array::new()))
            .as_array_mut()
            .ok_or_else(|| format!("upstream.{pool}.{key} is not an array"))?;

        if backends
            .iter()
            .any(|backend| backend_address(backend) == Some(address))
        {
            return Err(format!("{address} is already in pool {pool}"));
        }

        if weight == 1 {
            backends.push(address);
        } else {
            let mut entry = InlineTable::new();
            entry.insert("address", address.into());
            entry.insert("weight", i64::from(weight).into());
            backends.push(entry);
        }
        Ok(())
    }

    /// Removes a backend from `[upstream.POOL].backends`.
    pub(crate) fn remove_backend(&mut self, pool: &str, address: &str) -> Result<(), String> {
        let table = self.pool_mut(pool)?;
        for key in ["backends", "to"] {
            if let Some(backends) = table.get_mut(key).and_then(Item::as_array_mut) {
                let before = backends.len();
                backends.retain(|backend| backend_address(backend) != Some(address));
                if backends.len() != before {
                    return Ok(());
                }
            }
        }
        Err(format!("{address} is not in pool {pool}"))
    }

    /// Adds an `[upstream.NAME]` table.
    pub(crate) fn add_pool(&mut self, name: &str, spec: &PoolSpec) -> Result<(), String> {
        spec.policy.parse::<Policy>()?;

        let upstreams = self
            .document
            .entry("upstream")
            .or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(true);
                Item::Table(table)
            })
            .as_table_mut()
            .ok_or("`upstream` is not a table")?;

        if upstreams.contains_key(name) {
            return Err(format!("pool {name} already exists"));
        }

        let mut pool = Table::new();
        pool.insert("policy", value(spec.policy.as_str()));
        if let Some(key) = &spec.key {
            pool.insert("key", value(key.as_str()));
        }
        if let Some(retries) = spec.retries {
            pool.insert("retries", value(i64::from(retries)));
        }
        let backends: Array = spec.backends.iter().map(String::as_str).collect();
        pool.insert("backends", value(backends));

        if spec.health_uri.is_some() || spec.health_interval.is_some() {
            let mut health = Table::new();
            if let Some(uri) = &spec.health_uri {
                health.insert("uri", value(uri.as_str()));
            }
            if let Some(interval) = &spec.health_interval {
                health.insert("interval", value(interval.as_str()));
            }
            pool.insert("health", Item::Table(health));
        }
        if let Some(fail_duration) = &spec.fail_duration {
            let mut passive = Table::new();
            passive.insert("fail_duration", value(fail_duration.as_str()));
            pool.insert("passive", Item::Table(passive));
        }

        upstreams.insert(name, Item::Table(pool));
        Ok(())
    }

    /// Deletes an `[upstream.NAME]` table.
    pub(crate) fn remove_pool(&mut self, name: &str) -> Result<(), String> {
        let upstreams = self
            .document
            .get_mut("upstream")
            .and_then(Item::as_table_mut)
            .ok_or("no pools defined")?;
        upstreams
            .remove(name)
            .ok_or_else(|| format!("pool {name} is not defined"))?;
        Ok(())
    }

    /// Appends a `[[server]]`.
    pub(crate) fn add_service(&mut self, spec: &ServiceSpec) -> Result<(), String> {
        let mut server = Table::new();

        if let Some(name) = &spec.name {
            server.insert("name", value(name.as_str()));
        }
        let listen: Vec<String> = spec.listen.iter().map(ToString::to_string).collect();
        server.insert("listen", strings(&listen));
        if spec.protocol != "http" {
            server.insert("protocol", value(spec.protocol));
        }
        if let Some(uri) = &spec.uri {
            server.insert("uri", value(uri.as_str()));
        }
        match (&spec.serve, spec.to.as_slice()) {
            (Some(root), []) => {
                server.insert("serve", value(root.as_str()));
            }
            (None, []) => return Err(String::from("a service needs --to or --serve")),
            (None, targets) => {
                server.insert("forward", strings(targets));
            }
            (Some(_), _) => return Err(String::from("use either --to or --serve")),
        }

        let servers = self
            .document
            .entry("server")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .ok_or("`server` is not an array of tables")?;
        servers.push(server);
        Ok(())
    }

    /// Removes services whose name or one of whose listen addresses matches.
    pub(crate) fn remove_service(&mut self, service: &str) -> Result<usize, String> {
        let servers = self
            .document
            .get_mut("server")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or("no services defined")?;

        let before = servers.len();
        servers.retain(|server| !service_matches(server, service));
        let removed = before - servers.len();
        if removed == 0 {
            return Err(format!("no service named or listening on {service}"));
        }
        Ok(removed)
    }

    /// Validates the edited document and writes it atomically.
    pub(crate) fn save(&self) -> Result<Config, String> {
        let text = self.document.to_string();
        let config = Config::from_toml(&text)
            .map_err(|err| format!("the change would break the configuration: {err}"))?;

        let temporary = self.path.with_extension("toml.tmp");
        fs::write(&temporary, &text).map_err(|err| format!("{}: {err}", temporary.display()))?;
        fs::rename(&temporary, &self.path)
            .map_err(|err| format!("{}: {err}", self.path.display()))?;
        Ok(config)
    }
}

/// Value as a string when there is one entry, as an array otherwise.
fn strings(values: &[String]) -> Item {
    match values {
        [single] => value(single.as_str()),
        many => value(many.iter().map(String::as_str).collect::<Array>()),
    }
}

fn backend_address(backend: &Value) -> Option<&str> {
    match backend {
        Value::String(address) => Some(address.value()),
        Value::InlineTable(table) => table.get("address").and_then(Value::as_str),
        _ => None,
    }
}

fn service_matches(server: &Table, service: &str) -> bool {
    if server.get("name").and_then(Item::as_str) == Some(service) {
        return true;
    }
    match server.get("listen").and_then(Item::as_value) {
        Some(Value::String(listen)) => listen.value() == service,
        Some(Value::Array(listen)) => listen
            .iter()
            .any(|address| address.as_str() == Some(service)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
# Main API pool. This comment must survive edits.
[upstream.api]
policy = "least_conn"
backends = ["127.0.0.1:8080"]

[[server]]
name = "web"
listen = "127.0.0.1:8000"
forward = "@api"
"#;

    #[test]
    fn backends_are_added_and_removed_keeping_comments() {
        let mut file = ConfigFile::from_text(BASE);
        file.add_backend("api", "127.0.0.1:8081", 1).unwrap();
        file.add_backend("api", "127.0.0.1:8082", 3).unwrap();
        assert!(file.add_backend("api", "127.0.0.1:8081", 1).is_err());
        assert!(file.add_backend("missing", "127.0.0.1:1", 1).is_err());

        let text = file.document.to_string();
        assert!(text.contains("This comment must survive edits."));
        let config = Config::from_toml(&text).unwrap();
        let backends = &config.upstreams["api"].backends;
        assert_eq!(backends.len(), 3);
        assert_eq!(backends[2].weight, 3);

        file.remove_backend("api", "127.0.0.1:8082").unwrap();
        assert!(file.remove_backend("api", "127.0.0.1:8082").is_err());
        let config = Config::from_toml(&file.document.to_string()).unwrap();
        assert_eq!(config.upstreams["api"].backends.len(), 2);
    }

    #[test]
    fn pools_and_services_round_trip() {
        let mut file = ConfigFile::from_text(BASE);
        file.add_pool(
            "db",
            &PoolSpec {
                policy: String::from("ip_hash"),
                backends: vec![String::from("10.0.0.1:5432")],
                health_interval: Some(String::from("5s")),
                fail_duration: Some(String::from("30s")),
                ..PoolSpec::default()
            },
        )
        .unwrap();
        assert!(
            file.add_pool(
                "db",
                &PoolSpec {
                    policy: "random".into(),
                    ..PoolSpec::default()
                }
            )
            .is_err()
        );
        assert!(
            file.add_pool(
                "x",
                &PoolSpec {
                    policy: "nope".into(),
                    ..PoolSpec::default()
                }
            )
            .is_err()
        );

        file.add_service(&ServiceSpec {
            listen: vec!["127.0.0.1:5432".parse().unwrap()],
            protocol: "tcp",
            to: vec![String::from("@db")],
            name: Some(String::from("postgres")),
            ..ServiceSpec::default()
        })
        .unwrap();

        let text = file.document.to_string();
        let config = Config::from_toml(&text).unwrap_or_else(|err| panic!("{err}\n{text}"));
        assert_eq!(config.servers.len(), 2);
        assert!(config.upstreams["db"].health.is_some());
        assert!(
            !text.contains("[upstream]\n"),
            "parent table must stay implicit:\n{text}"
        );

        assert_eq!(file.remove_service("postgres").unwrap(), 1);
        assert_eq!(file.remove_service("127.0.0.1:8000").unwrap(), 1);
        file.remove_pool("db").unwrap();
        assert!(file.remove_pool("db").is_err());
    }

    #[test]
    fn empty_file_gets_tables_created() {
        let mut file = ConfigFile::from_text("");
        file.add_pool(
            "api",
            &PoolSpec {
                policy: "random".into(),
                ..PoolSpec::default()
            },
        )
        .unwrap();
        file.add_service(&ServiceSpec {
            listen: vec!["127.0.0.1:9000".parse().unwrap()],
            protocol: "http",
            to: vec![String::from("@api")],
            ..ServiceSpec::default()
        })
        .unwrap();
        let text = file.document.to_string();
        Config::from_toml(&text).unwrap_or_else(|err| panic!("{err}\n{text}"));
    }
}
