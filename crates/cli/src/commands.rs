//! Implementation of the client side commands (everything except `run` and
//! `start`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rxh_core::config::{Action, Forward, Policy};
use serde_json::{Value, json};

use crate::cli::{
    Cli, Command, ConfigCommand, PoolCommand, ServiceCommand, ServiceProtocol, UpstreamCommand,
};
use crate::client::{AdminClient, ClientError};
use crate::edit::{self, ConfigFile, PoolSpec, ServiceSpec};
use crate::process::{self, DaemonSignal};

/// Default configuration file.
pub(crate) const DEFAULT_CONFIG: &str = "rxh.toml";

/// Config path for `run`, `start` and `validate`.
pub(crate) fn config_path(cli: &Cli) -> PathBuf {
    cli.config
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
}

/// Admin address: `--admin`, else `[admin] listen` of the config file, else
/// the default.
fn admin_address(cli: &Cli) -> SocketAddr {
    if let Some(address) = cli.admin {
        return address;
    }
    rxh_server::load_config(&config_path(cli))
        .ok()
        .map(|config| config.admin.listen)
        .unwrap_or_else(|| rxh_core::config::Admin::default().listen)
}

/// Runs a client command.
pub(crate) async fn execute(cli: &Cli, command: Command) -> Result<(), String> {
    let client = AdminClient::new(admin_address(cli));

    match command {
        Command::Run
        | Command::Start { .. }
        | Command::FileServer { .. }
        | Command::ReverseProxy { .. } => unreachable!("handled in main"),
        Command::Config(ConfigCommand::Show { json }) => config_show(cli, &client, json).await,
        Command::Stop => stop(cli, &client).await,
        Command::Reload => reload(cli, &client).await,
        Command::Status { json } => status(&client, json).await,
        Command::Validate => validate(&config_path(cli)),
        Command::Policies => {
            policies();
            Ok(())
        }
        Command::Upstream(command) => upstream(cli, &client, command).await,
        Command::Pool(command) => pool(cli, &client, command).await,
        Command::Service(command) => service(cli, &client, command).await,
    }
}

async fn stop(cli: &Cli, client: &AdminClient) -> Result<(), String> {
    match client.post("/stop", None).await {
        Ok(_) => {
            for _ in 0..300 {
                if let Err(ClientError::Unreachable(_)) = client.get("/status").await {
                    println!("rxh stopped");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            println!("rxh is stopping, still draining connections");
            Ok(())
        }
        Err(ClientError::Unreachable(_)) => {
            let pid = process::signal(&cli.pid_file, DaemonSignal::Terminate).map_err(|err| {
                format!(
                    "no daemon on {} and no usable PID file: {err}",
                    client.address()
                )
            })?;
            println!("Sent SIGTERM to {pid}");
            Ok(())
        }
        Err(err) => Err(err.to_string()),
    }
}

async fn reload(cli: &Cli, client: &AdminClient) -> Result<(), String> {
    match client.post("/reload", None).await {
        Ok(_) => {
            println!("Configuration reloaded");
            Ok(())
        }
        Err(ClientError::Unreachable(_)) => {
            let pid = process::signal(&cli.pid_file, DaemonSignal::Reload).map_err(|err| {
                format!(
                    "no daemon on {} and no usable PID file: {err}",
                    client.address()
                )
            })?;
            println!("Sent SIGHUP to {pid}");
            Ok(())
        }
        Err(err) => Err(format!(
            "reload rejected, the running configuration is unchanged: {err}"
        )),
    }
}

async fn status(client: &AdminClient, raw: bool) -> Result<(), String> {
    let status = client.get("/status").await.map_err(|err| err.to_string())?;

    if raw {
        println!(
            "{}",
            serde_json::to_string_pretty(&status).map_err(|err| err.to_string())?
        );
        return Ok(());
    }

    println!(
        "rxh {}  pid {}  up {}s  admin {}",
        status["version"].as_str().unwrap_or("?"),
        status["pid"],
        status["uptime_seconds"],
        status["admin"].as_str().unwrap_or("?"),
    );
    if let Some(path) = status["config_path"].as_str() {
        println!("config {path}");
    }

    println!("\n{:<24} {:<6} {:<16} STATE", "LISTEN", "PROTO", "NAME");
    for server in status["servers"].as_array().into_iter().flatten() {
        println!(
            "{:<24} {:<6} {:<16} {}",
            server["listen"].as_str().unwrap_or("?"),
            server["protocol"].as_str().unwrap_or("?"),
            server["name"].as_str().unwrap_or("-"),
            server["state"].as_str().unwrap_or("?"),
        );
    }

    for pool in status["pools"].as_array().into_iter().flatten() {
        println!();
        print_pool(pool);
    }
    Ok(())
}

fn print_pool(pool: &Value) {
    println!(
        "pool {} ({})",
        pool["name"].as_str().unwrap_or("?"),
        pool["policy"].as_str().unwrap_or("?")
    );
    let upstreams = pool["upstreams"].as_array().cloned().unwrap_or_default();
    if upstreams.is_empty() {
        println!("  (no backends)");
        return;
    }
    println!(
        "  {:<28} {:>6} {:>8} {:>7} {:>9} {:>5}",
        "ADDRESS", "WEIGHT", "HEALTHY", "ACTIVE", "REQUESTS", "FAILS"
    );
    for upstream in upstreams {
        let health = match (
            upstream["healthy"].as_bool(),
            upstream["available"].as_bool(),
        ) {
            (Some(true), Some(true)) => "yes",
            (Some(true), _) => "full",
            (Some(false), _) => "NO",
            _ => "?",
        };
        println!(
            "  {:<28} {:>6} {:>8} {:>7} {:>9} {:>5}",
            upstream["address"].as_str().unwrap_or("?"),
            upstream["weight"],
            health,
            upstream["num_requests"],
            upstream["total_requests"],
            upstream["fails"],
        );
    }
}

fn validate(path: &Path) -> Result<(), String> {
    let config = rxh_server::load_config(path).map_err(|err| err.to_string())?;
    let listeners: usize = config
        .servers
        .iter()
        .map(|server| server.listen.len())
        .sum();
    println!(
        "{}: OK ({} services, {listeners} listeners, {} pools)",
        path.display(),
        config.servers.len(),
        config.upstreams.len()
    );
    Ok(())
}

fn policies() {
    let descriptions = [
        (Policy::Random, "random available backend"),
        (
            Policy::RandomChoose,
            "sample `choose` (default 2) at random, take the least loaded",
        ),
        (Policy::First, "first available backend in order (failover)"),
        (Policy::RoundRobin, "rotate through backends"),
        (
            Policy::WeightedRoundRobin,
            "smooth weighted rotation (default)",
        ),
        (
            Policy::LeastConn,
            "fewest requests or connections in flight",
        ),
        (Policy::IpHash, "hash of the peer IP"),
        (
            Policy::ClientIpHash,
            "hash of the real client IP (trusted_proxies)",
        ),
        (Policy::UriHash, "hash of path and query (http)"),
        (Policy::Query, "hash of query parameter `key` (http)"),
        (Policy::Header, "hash of header `key` (http)"),
        (
            Policy::Cookie,
            "sticky sessions via signed cookie `key` (http)",
        ),
    ];
    for (policy, description) in descriptions {
        println!("{:<22} {description}", policy.name());
    }
}

/// File to edit: `--config`, else the daemon's file, else the default.
async fn editable_config(cli: &Cli, client: &AdminClient) -> PathBuf {
    if let Some(path) = &cli.config {
        return path.clone();
    }
    match client.get("/status").await {
        Ok(status) => status["config_path"]
            .as_str()
            .map_or_else(|| PathBuf::from(DEFAULT_CONFIG), PathBuf::from),
        Err(_) => PathBuf::from(DEFAULT_CONFIG),
    }
}

/// The main file followed by the files it includes. Just the main file when
/// it does not exist yet.
fn config_files(main: &Path) -> Result<Vec<PathBuf>, String> {
    if !main.exists() {
        return Ok(vec![main.to_owned()]);
    }
    rxh_server::config_files(main).map_err(|err| err.to_string())
}

/// The file that defines pool `name`.
fn file_defining_pool(main: &Path, name: &str) -> Result<ConfigFile, String> {
    for path in config_files(main)? {
        let file = ConfigFile::open(&path)?;
        if file.defines_pool(name) {
            return Ok(file);
        }
    }
    Err(format!(
        "pool {name} is not defined in {} or its included files; create it with `rxh pool add {name}`",
        main.display()
    ))
}

/// Where a new pool or service is written: the main file, or `--file`.
struct WriteTarget {
    main: PathBuf,
    target: ConfigFile,
    /// The main file with a new `include` entry, when `--file` is not
    /// covered by it yet.
    main_file: Option<ConfigFile>,
}

impl WriteTarget {
    fn open(main: &Path, file: Option<&Path>) -> Result<Self, String> {
        let Some(file) = file else {
            return Ok(Self {
                main: main.to_owned(),
                target: ConfigFile::open(main)?,
                main_file: None,
            });
        };
        let directory = main.parent().unwrap_or(Path::new(""));
        let path = directory.join(file);
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap_or(path.to_owned());
        let target = canonical(&path);
        if target == canonical(main) {
            return Self::open(main, None);
        }

        let mut main_file = None;
        let patterns = ConfigFile::open(main)?.includes();
        let covered = rxh_server::include_covers(main, &patterns, &path)
            || config_files(main)?
                .iter()
                .any(|existing| canonical(existing) == target);
        if !covered {
            let base = canonical(if directory.as_os_str().is_empty() {
                Path::new(".")
            } else {
                directory
            });
            let entry = if file.is_absolute() {
                target.strip_prefix(&base).unwrap_or(file)
            } else {
                file
            };
            let mut main_edit = ConfigFile::open(main)?;
            main_edit.add_include(&entry.to_string_lossy())?;
            main_file = Some(main_edit);
        }
        Ok(Self {
            main: main.to_owned(),
            target: ConfigFile::open(&path)?,
            main_file,
        })
    }

    fn save(&self) -> Result<(), String> {
        let mut files = vec![&self.target];
        files.extend(self.main_file.as_ref());
        edit::save_all(&files, &self.main).map(|_| ())
    }
}

/// Prints the effective configuration.
async fn config_show(cli: &Cli, client: &AdminClient, json: bool) -> Result<(), String> {
    let config = match client.get("/config").await {
        Ok(config) => config,
        Err(ClientError::Unreachable(_)) => {
            let path = editable_config(cli, client).await;
            let config = rxh_server::load_config(&path).map_err(|err| err.to_string())?;
            serde_json::to_value(config).map_err(|err| err.to_string())?
        }
        Err(err) => return Err(err.to_string()),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?
        );
        return Ok(());
    }
    let table: toml::Value =
        serde_json::from_value(without_nulls(config)).map_err(|err| err.to_string())?;
    print!(
        "{}",
        toml::to_string_pretty(&table).map_err(|err| err.to_string())?
    );
    Ok(())
}

/// TOML has no null: unset options are left out.
fn without_nulls(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, without_nulls(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(without_nulls).collect()),
        other => other,
    }
}

/// Reloads the daemon after a configuration edit, if it runs.
async fn apply(client: &AdminClient) -> Result<(), String> {
    match client.post("/reload", None).await {
        Ok(_) => {
            println!("Daemon reloaded");
            Ok(())
        }
        Err(ClientError::Unreachable(_)) => {
            println!("Daemon not running, the change applies on the next start");
            Ok(())
        }
        Err(err) => Err(format!("saved, but the daemon rejected the reload: {err}")),
    }
}

async fn upstream(cli: &Cli, client: &AdminClient, command: UpstreamCommand) -> Result<(), String> {
    match command {
        UpstreamCommand::List { pool } => {
            let path = match &pool {
                Some(pool) => format!("/pools/{}", encode(pool)),
                None => String::from("/pools"),
            };
            match client.get(&path).await {
                Ok(Value::Array(pools)) => pools.iter().for_each(print_pool),
                Ok(pool) => print_pool(&pool),
                Err(ClientError::Unreachable(_)) => {
                    println!("(daemon not running, showing the configuration file)\n");
                    list_pools_from_file(&editable_config(cli, client).await, pool.as_deref())?;
                }
                Err(err) => return Err(err.to_string()),
            }
            Ok(())
        }
        UpstreamCommand::Add {
            pool,
            address,
            weight,
            persistence,
        } => {
            if !persistence.no_save {
                let main = editable_config(cli, client).await;
                let mut file = file_defining_pool(&main, &pool)?;
                file.add_backend(&pool, &address, weight)?;
                edit::save_all(&[&file], &main)?;
                println!(
                    "Saved {address} to pool {pool} in {}",
                    file.path().display()
                );
            }
            let body = json!({ "address": address, "weight": weight });
            match client
                .post(&format!("/pools/{}/upstreams", encode(&pool)), Some(body))
                .await
            {
                Ok(_) => println!("Added {address} to pool {pool} on the running daemon"),
                Err(ClientError::Unreachable(_)) if !persistence.no_save => {
                    println!("Daemon not running, the change applies on the next start");
                }
                Err(err) => return Err(err.to_string()),
            }
            Ok(())
        }
        UpstreamCommand::Remove {
            pool,
            address,
            persistence,
        } => {
            if !persistence.no_save {
                let main = editable_config(cli, client).await;
                let mut file = file_defining_pool(&main, &pool)?;
                file.remove_backend(&pool, &address)?;
                edit::save_all(&[&file], &main)?;
                println!(
                    "Removed {address} from pool {pool} in {}",
                    file.path().display()
                );
            }
            let body = json!({ "address": address });
            match client
                .delete(&format!("/pools/{}/upstreams", encode(&pool)), body)
                .await
            {
                Ok(_) => println!("Removed {address} from pool {pool} on the running daemon"),
                Err(ClientError::Unreachable(_)) if !persistence.no_save => {
                    println!("Daemon not running, the change applies on the next start");
                }
                Err(err) => return Err(err.to_string()),
            }
            Ok(())
        }
    }
}

fn list_pools_from_file(path: &Path, only: Option<&str>) -> Result<(), String> {
    let config = rxh_server::load_config(path).map_err(|err| err.to_string())?;
    for (name, group) in &config.upstreams {
        if only.is_some_and(|only| only != name) {
            continue;
        }
        println!("pool {name} ({})", group.policy);
        for backend in &group.backends {
            println!("  {:<28} weight {}", backend.address, backend.weight);
        }
    }
    Ok(())
}

async fn pool(cli: &Cli, client: &AdminClient, command: PoolCommand) -> Result<(), String> {
    match command {
        PoolCommand::List => match client.get("/pools").await {
            Ok(pools) => {
                for pool in pools.as_array().into_iter().flatten() {
                    let count = pool["upstreams"].as_array().map_or(0, Vec::len);
                    println!(
                        "{:<24} {:<22} {count} backends",
                        pool["name"].as_str().unwrap_or("?"),
                        pool["policy"].as_str().unwrap_or("?")
                    );
                }
                Ok(())
            }
            Err(ClientError::Unreachable(_)) => {
                list_pools_from_file(&editable_config(cli, client).await, None)
            }
            Err(err) => Err(err.to_string()),
        },
        PoolCommand::Add {
            name,
            policy,
            backends,
            key,
            retries,
            health_uri,
            health_interval,
            fail_duration,
            file,
        } => {
            let mut write =
                WriteTarget::open(&editable_config(cli, client).await, file.as_deref())?;
            let spec = PoolSpec {
                policy,
                backends,
                key,
                retries,
                health_uri,
                health_interval,
                fail_duration,
            };
            write.target.add_pool(&name, &spec)?;
            write.save()?;
            println!(
                "Created pool {name} in {}. Use it with `forward = \"@{name}\"`",
                write.target.path().display()
            );
            apply(client).await
        }
        PoolCommand::Remove { name } => {
            let main = editable_config(cli, client).await;
            let mut file = file_defining_pool(&main, &name)?;
            file.remove_pool(&name)?;
            edit::save_all(&[&file], &main)?;
            println!("Deleted pool {name} from {}", file.path().display());
            apply(client).await
        }
    }
}

async fn service(cli: &Cli, client: &AdminClient, command: ServiceCommand) -> Result<(), String> {
    match command {
        ServiceCommand::List => {
            let path = editable_config(cli, client).await;
            let config = rxh_server::load_config(&path).map_err(|err| err.to_string())?;
            println!("{:<16} {:<6} {:<36} TARGET", "NAME", "PROTO", "LISTEN");
            for server in &config.servers {
                let listen: Vec<String> = server.listen.iter().map(ToString::to_string).collect();
                let targets: Vec<String> = server.patterns.iter().map(describe_target).collect();
                println!(
                    "{:<16} {:<6} {:<36} {}",
                    server.name.as_deref().unwrap_or("-"),
                    server.protocol,
                    listen.join(","),
                    targets.join(", ")
                );
            }
            Ok(())
        }
        ServiceCommand::Add {
            listen,
            protocol,
            to,
            serve,
            name,
            uri,
            host,
            file,
        } => {
            let mut write =
                WriteTarget::open(&editable_config(cli, client).await, file.as_deref())?;
            let protocol = match protocol {
                ServiceProtocol::Http => "http",
                ServiceProtocol::Tcp => "tcp",
                ServiceProtocol::Udp => "udp",
            };
            let serve = match serve {
                Some(dir) => Some(
                    std::fs::canonicalize(&dir)
                        .map_err(|err| format!("{}: {err}", dir.display()))?
                        .to_string_lossy()
                        .into_owned(),
                ),
                None => None,
            };
            let spec = ServiceSpec {
                listen,
                protocol,
                to,
                serve,
                name,
                uri,
                host,
            };
            write.target.add_service(&spec)?;
            write.save()?;
            println!("Added service to {}", write.target.path().display());
            apply(client).await
        }
        ServiceCommand::Remove { service } => {
            let main = editable_config(cli, client).await;
            for path in config_files(&main)? {
                let mut file = ConfigFile::open(&path)?;
                let Ok(removed) = file.remove_service(&service) else {
                    continue;
                };
                edit::save_all(&[&file], &main)?;
                println!(
                    "Removed {removed} service(s) from {}",
                    file.path().display()
                );
                return apply(client).await;
            }
            Err(format!("no service named or listening on {service}"))
        }
    }
}

fn describe_target(pattern: &rxh_core::config::Pattern) -> String {
    let target = match &pattern.action {
        Action::Forward(Forward::Group(name)) => format!("@{name}"),
        Action::Forward(Forward::Inline(group)) => {
            let addresses: Vec<&str> = group.backends.iter().map(|b| b.address.as_str()).collect();
            addresses.join(" ")
        }
        Action::Serve(files) => format!("files {}", files.root),
        Action::Redirect(redirect) => format!("redirect {}", redirect.to),
    };
    if pattern.uri == "/" {
        target
    } else {
        format!("{} -> {target}", pattern.uri)
    }
}

/// Percent-encodes a path segment.
fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool_spec(backend: &str) -> PoolSpec {
        PoolSpec {
            policy: String::from("random"),
            backends: vec![backend.to_owned()],
            ..PoolSpec::default()
        }
    }

    #[test]
    fn file_covered_by_a_glob_is_not_added_to_include() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("rxh.toml");
        std::fs::write(&main, "include = [\"conf.d/*.toml\"]\n").expect("write");

        let mut write =
            WriteTarget::open(&main, Some(Path::new("conf.d/new.toml"))).expect("opens");
        assert!(write.main_file.is_none(), "covered by the glob");
        write
            .target
            .add_pool("api", &pool_spec("127.0.0.1:1"))
            .expect("adds");
        write.save().expect("saves");
        assert_eq!(
            std::fs::read_to_string(&main).expect("read"),
            "include = [\"conf.d/*.toml\"]\n"
        );

        let write = WriteTarget::open(&main, Some(Path::new("other/new.toml"))).expect("opens");
        assert!(write.main_file.is_some(), "not covered");
    }

    #[test]
    fn failed_validation_restores_every_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("rxh.toml");
        let main_text = "# keep me\ninclude = [\"conf.d/*.toml\"]\n";
        let api_text = "[upstream.api]\nbackends = [\"127.0.0.1:1\"]\n";
        std::fs::write(&main, main_text).expect("write");
        std::fs::create_dir(dir.path().join("conf.d")).expect("mkdir");
        std::fs::write(dir.path().join("conf.d/api.toml"), api_text).expect("write");

        // `api` is already defined in conf.d/api.toml, so the merge fails.
        let mut write = WriteTarget::open(&main, Some(Path::new("extra/dup.toml"))).expect("opens");
        assert!(write.main_file.is_some());
        write
            .target
            .add_pool("api", &pool_spec("127.0.0.1:2"))
            .expect("adds");
        let err = write.save().expect_err("duplicate pool");
        assert!(err.contains("pool api"), "{err}");
        assert!(!err.contains("rollback failed"), "{err}");

        assert_eq!(std::fs::read_to_string(&main).expect("read"), main_text);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("conf.d/api.toml")).expect("read"),
            api_text
        );
        assert!(
            !dir.path().join("extra").exists(),
            "created directory removed"
        );
    }

    #[test]
    fn encodes_inline_pool_names() {
        assert_eq!(encode("server[0]#1"), "server%5B0%5D%231");
        assert_eq!(encode("api"), "api");
    }
}
