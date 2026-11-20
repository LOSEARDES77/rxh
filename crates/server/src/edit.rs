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
pub struct PoolSpec {
    /// Load balancing policy, as written in `policy`.
    pub policy: String,
    /// Backend addresses.
    pub backends: Vec<String>,
    /// Hash key of the policies that take one.
    pub key: Option<String>,
    /// Attempts on other backends after a failure.
    pub retries: Option<u32>,
    /// URI of the active health check.
    pub health_uri: Option<String>,
    /// Interval of the active health check, as a duration.
    pub health_interval: Option<String>,
    /// How long passive health checks keep a failed backend out.
    pub fail_duration: Option<String>,
}

/// New service definition.
#[derive(Debug, Default)]
pub struct ServiceSpec {
    /// Listen addresses.
    pub listen: Vec<SocketAddr>,
    /// `http`, `tcp` or `udp`; `http` is left implicit.
    pub protocol: &'static str,
    /// Forward targets: addresses or `@pool`.
    pub to: Vec<String>,
    /// Directory served as static files instead of forwarding.
    pub serve: Option<String>,
    /// Name of the service.
    pub name: Option<String>,
    /// Path the service matches.
    pub uri: Option<String>,
    /// Host names the service matches.
    pub host: Vec<String>,
}

/// A configuration file loaded for editing.
pub struct ConfigFile {
    path: PathBuf,
    document: DocumentMut,
    /// Text on disk when opened, `None` if the file did not exist.
    original: Option<String>,
}

impl ConfigFile {
    /// Loads `path`. A missing file starts as an empty document.
    pub fn open(path: &Path) -> Result<Self, String> {
        let original = match fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(format!("{}: {err}", path.display())),
        };
        let document = original
            .as_deref()
            .unwrap_or_default()
            .parse()
            .map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Self {
            path: path.to_owned(),
            document,
            original,
        })
    }

    #[cfg(test)]
    fn from_text(text: &str) -> Self {
        Self {
            path: PathBuf::from("test.toml"),
            document: text.parse().expect("valid TOML"),
            original: Some(text.to_owned()),
        }
    }

    /// Path of the file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The edited document as TOML text.
    pub fn text(&self) -> String {
        self.document.to_string()
    }

    /// Text on disk when the file was opened, `None` if it did not exist.
    pub fn original(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// The edited document.
    pub fn document(&self) -> &DocumentMut {
        &self.document
    }

    /// The edited document, for changes the methods of this type do not cover.
    pub fn document_mut(&mut self) -> &mut DocumentMut {
        &mut self.document
    }

    /// Replaces the whole document with `text`. On a parse error the document
    /// is left unchanged and the error names the file.
    pub fn set_text(&mut self, text: &str) -> Result<(), String> {
        self.document = text
            .parse()
            .map_err(|err| format!("{}: {err}", self.path.display()))?;
        Ok(())
    }

    /// True if `[upstream.NAME]` is defined in this file.
    pub fn defines_pool(&self, name: &str) -> bool {
        self.document
            .get("upstream")
            .and_then(|upstreams| upstreams.get(name))
            .is_some_and(Item::is_table)
    }

    /// Entries of the top level `include` list.
    pub fn includes(&self) -> Vec<String> {
        self.document
            .get("include")
            .and_then(Item::as_array)
            .map(|include| {
                include
                    .iter()
                    .filter_map(|entry| entry.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Appends `path` to the top level `include` list unless it is there.
    pub fn add_include(&mut self, path: &str) -> Result<(), String> {
        let include = self
            .document
            .entry("include")
            .or_insert_with(|| value(Array::new()))
            .as_array_mut()
            .ok_or("`include` is not an array")?;
        if !include.iter().any(|entry| entry.as_str() == Some(path)) {
            include.push(path);
        }
        Ok(())
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
    pub fn add_backend(&mut self, pool: &str, address: &str, weight: u32) -> Result<(), String> {
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
    pub fn remove_backend(&mut self, pool: &str, address: &str) -> Result<(), String> {
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
    pub fn add_pool(&mut self, name: &str, spec: &PoolSpec) -> Result<(), String> {
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
    pub fn remove_pool(&mut self, name: &str) -> Result<(), String> {
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
    pub fn add_service(&mut self, spec: &ServiceSpec) -> Result<(), String> {
        let mut server = Table::new();

        if let Some(name) = &spec.name {
            server.insert("name", value(name.as_str()));
        }
        let listen: Vec<String> = spec.listen.iter().map(ToString::to_string).collect();
        server.insert("listen", strings(&listen));
        if spec.protocol != "http" {
            server.insert("protocol", value(spec.protocol));
        }
        match spec.host.as_slice() {
            [] => {}
            [host] => {
                server.insert("host", value(host.as_str()));
            }
            hosts => {
                server.insert("host", strings(hosts));
            }
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
    pub fn remove_service(&mut self, service: &str) -> Result<usize, String> {
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
}

/// Why [`try_save_all`] did not save.
#[derive(Debug)]
#[must_use]
pub enum SaveError {
    /// The configuration would not be valid with the change. Every file is
    /// as it was.
    Invalid(String),
    /// A file could not be written, or could not be put back.
    Io(String),
}

/// Writes the edited `files`, then loads the whole configuration starting at
/// `main` (includes merged) to validate it. If that fails every file goes
/// back to what it was and the error is returned. A file that cannot be
/// written stops the save before validation; the error names it.
pub fn save_all(files: &[&ConfigFile], main: &Path) -> Result<Config, String> {
    try_save_all(files, main).map_err(|err| match err {
        SaveError::Invalid(message) | SaveError::Io(message) => message,
    })
}

/// [`save_all`] with an error that tells an invalid configuration from a
/// file system failure.
pub fn try_save_all(files: &[&ConfigFile], main: &Path) -> Result<Config, SaveError> {
    // Topmost directory each new file needs, removed again on rollback.
    let created_directories: Vec<PathBuf> = files
        .iter()
        .filter(|file| file.original.is_none())
        .filter_map(|file| {
            let mut missing = None;
            for ancestor in file.path.ancestors().skip(1) {
                if ancestor.as_os_str().is_empty() || ancestor.exists() {
                    break;
                }
                missing = Some(ancestor.to_owned());
            }
            missing
        })
        .collect();

    // Files before this index were replaced and are the ones to put back.
    let mut written: usize = 0;
    let mut unwritable: Option<String> = None;
    for file in files {
        if let Err(err) = write_atomically(&file.path, &file.document.to_string()) {
            unwritable = Some(format!(
                "the configuration file {} cannot be written, nothing was changed: {err}",
                file.path.display()
            ));
            break;
        }
        written += 1;
    }
    let (mut message, is_invalid) = match unwritable {
        Some(message) => (message, false),
        None => match crate::load_config(main) {
            Ok(config) => return Ok(config),
            Err(err) => (
                format!("the change would break the configuration: {err}"),
                true,
            ),
        },
    };

    let mut failures: Vec<String> = Vec::new();
    if let Err(err) = restore(&files[..written]) {
        failures.push(err);
    }
    for directory in created_directories {
        if let Err(err) = fs::remove_dir_all(&directory)
            && err.kind() != io::ErrorKind::NotFound
        {
            failures.push(format!("{}: {err}", directory.display()));
        }
    }

    if !failures.is_empty() {
        message.push_str(&format!(
            "\nrollback failed, check these files by hand: {}",
            failures.join("; ")
        ));
    }
    Err(if is_invalid && failures.is_empty() {
        SaveError::Invalid(message)
    } else {
        SaveError::Io(message)
    })
}

/// Writes every file back to the text it had when opened, and removes the
/// files that did not exist then. Tries every file even after a failure; the
/// error lists each file that could not be restored.
pub fn restore(files: &[&ConfigFile]) -> Result<(), String> {
    let mut failures: Vec<String> = Vec::new();
    for file in files {
        let restored = match &file.original {
            Some(text) => write_atomically(&file.path, text),
            None => match fs::remove_file(&file.path) {
                Err(err) if err.kind() != io::ErrorKind::NotFound => {
                    Err(format!("{}: {err}", file.path.display()))
                }
                _ => Ok(()),
            },
        };
        if let Err(err) = restored {
            failures.push(err);
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(|err| format!("{}: {err}", directory.display()))?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    write_like(&temporary, path, text).map_err(|err| format!("{}: {err}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|err| format!("{}: {err}", path.display()))
}

/// Writes `text` to the new file `temporary`, which gets the mode and, when
/// the process may give it, the owner of `like`. Without `like` only the
/// owner can read it: a configuration file can hold secrets. The file is
/// private before anything is written to it.
#[cfg(unix)]
fn write_like(temporary: &Path, like: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{self as unix_fs, MetadataExt, OpenOptionsExt, PermissionsExt};

    // One left by a process that died would keep its own mode.
    match fs::remove_file(temporary) {
        Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(temporary)?;
    if let Ok(metadata) = fs::metadata(like) {
        // Only root can give a file away; the mode is what protects it.
        let _ = unix_fs::fchown(&file, Some(metadata.uid()), Some(metadata.gid()));
        file.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o7777))?;
    }
    file.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_like(temporary: &Path, _like: &Path, text: &str) -> io::Result<()> {
    fs::write(temporary, text)
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

    #[test]
    fn restore_puts_back_the_text_and_removes_new_files() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("a.toml");
        fs::write(&existing, "a = 1").unwrap();
        let mut file = ConfigFile::open(&existing).unwrap();
        file.set_text("a = 2").unwrap();
        assert!(file.set_text("a = ").unwrap_err().contains("a.toml"));
        assert_eq!(file.original(), Some("a = 1"));
        fs::write(&existing, file.text()).unwrap();

        let missing = dir.path().join("new.toml");
        let new = ConfigFile::open(&missing).unwrap();
        assert_eq!(new.original(), None);
        fs::write(&missing, "b = 1").unwrap();

        restore(&[&file, &new]).unwrap();
        assert_eq!(fs::read_to_string(&existing).unwrap(), "a = 1");
        assert!(!missing.exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_save_keeps_the_mode_of_the_file() {
        use std::os::unix::fs::PermissionsExt;

        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("rxh.toml");
        fs::write(&main, BASE).unwrap();
        // A temporary file left behind, readable by everybody.
        fs::write(dir.path().join("rxh.toml.tmp"), "stale").unwrap();

        for wanted in [0o600, 0o640] {
            fs::set_permissions(&main, fs::Permissions::from_mode(wanted)).unwrap();

            let mut file = ConfigFile::open(&main).unwrap();
            file.add_backend("api", "127.0.0.1:8081", 1).unwrap();
            save_all(&[&file], &main).unwrap();
            assert_eq!(mode(&main), wanted, "after a save");
            assert!(fs::read_to_string(&main).unwrap().contains("8081"));

            let before = fs::read_to_string(&main).unwrap();
            let mut file = ConfigFile::open(&main).unwrap();
            file.set_text("[upstream.api]\npolicy = \"nope\"\n")
                .unwrap();
            save_all(&[&file], &main).unwrap_err();
            assert_eq!(mode(&main), wanted, "after a rejected save");
            assert_eq!(fs::read_to_string(&main).unwrap(), before);
            file.remove_backend("api", "127.0.0.1:8081").unwrap_err();
            fs::write(&main, BASE).unwrap();
        }

        let new = dir.path().join("new.toml");
        write_atomically(&new, "a = 1").unwrap();
        assert_eq!(mode(&new), 0o600, "a new file is private");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_written_is_named_and_the_others_are_put_back() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("rxh.toml");
        let main_text = format!("include = [\"locked/*.toml\"]\n{BASE}");
        fs::write(&main, &main_text).unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        let extra = locked.join("extra.toml");
        fs::write(&extra, "").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        if fs::write(locked.join("probe"), "").is_ok() {
            // Root writes anywhere: nothing to check.
            return;
        }

        let mut first = ConfigFile::open(&main).unwrap();
        first.add_backend("api", "127.0.0.1:8081", 1).unwrap();
        let mut second = ConfigFile::open(&extra).unwrap();
        second.set_text("[upstream.other]\n").unwrap();
        let err = save_all(&[&first, &second], &main).unwrap_err();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

        let expected = format!(
            "the configuration file {} cannot be written, nothing was changed: ",
            extra.display()
        );
        assert!(err.starts_with(&expected), "{err}");
        assert!(err.contains("Permission denied"), "{err}");
        assert!(!err.contains("would break"), "{err}");
        assert!(!err.contains("rollback failed"), "{err}");
        assert_eq!(fs::read_to_string(&main).unwrap(), main_text);
        assert_eq!(fs::read_to_string(&extra).unwrap(), "");
    }

    #[test]
    fn service_hosts_are_written() {
        let mut file = ConfigFile::from_text("");
        file.add_service(&ServiceSpec {
            listen: vec!["127.0.0.1:9000".parse().unwrap()],
            protocol: "http",
            to: vec![String::from("a:1")],
            host: vec![String::from("a.test"), String::from("*.b.test")],
            ..ServiceSpec::default()
        })
        .unwrap();
        let text = file.document.to_string();
        let config = Config::from_toml(&text).unwrap_or_else(|err| panic!("{err}\n{text}"));
        assert_eq!(config.servers[0].host, ["a.test", "*.b.test"]);
    }
}
