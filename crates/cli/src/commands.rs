//! Implementation of the client side commands (everything except `run` and
//! `start`).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rxh_core::config::{Action, Forward, Policy};
use serde_json::{Value, json};

use crate::cli::{Cli, Command, PoolCommand, ServiceCommand, ServiceProtocol, UpstreamCommand};
use crate::client::{AdminClient, ClientError};
use crate::edit::{ConfigFile, PoolSpec, ServiceSpec};
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
        Command::Run | Command::Start { .. } => unreachable!("handled in main"),
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
            serde_json::to_string_pretty(&status).unwrap_or_default()
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
                let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
                file.add_backend(&pool, &address, weight)?;
                file.save()?;
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
                let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
                file.remove_backend(&pool, &address)?;
                file.save()?;
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
        } => {
            let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
            let spec = PoolSpec {
                policy,
                backends,
                key,
                retries,
                health_uri,
                health_interval,
                fail_duration,
            };
            file.add_pool(&name, &spec)?;
            file.save()?;
            println!(
                "Created pool {name} in {}. Use it with `forward = \"@{name}\"`",
                file.path().display()
            );
            apply(client).await
        }
        PoolCommand::Remove { name } => {
            let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
            file.remove_pool(&name)?;
            file.save()?;
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
        } => {
            let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
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
            file.add_service(&spec)?;
            file.save()?;
            println!("Added service to {}", file.path().display());
            apply(client).await
        }
        ServiceCommand::Remove { service } => {
            let mut file = ConfigFile::open(&editable_config(cli, client).await)?;
            let removed = file.remove_service(&service)?;
            file.save()?;
            println!(
                "Removed {removed} service(s) from {}",
                file.path().display()
            );
            apply(client).await
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

    #[test]
    fn encodes_inline_pool_names() {
        assert_eq!(encode("server[0]#1"), "server%5B0%5D%231");
        assert_eq!(encode("api"), "api");
    }
}
