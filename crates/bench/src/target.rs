//! Proxies under test: how to find, configure, start and stop rxh and Caddy.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};

/// Why a scenario could not produce a measurement.
#[derive(Debug)]
pub enum Problem {
    /// The target cannot run this scenario at all (reported as `n/a`).
    Unsupported(String),

    /// Something broke (reported as `FAIL`).
    Failed(String),
}

/// Which proxy a [`Target`] is. Decides the config format and command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `rxh run --config rxh.toml`.
    Rxh,

    /// `caddy run --adapter caddyfile --config Caddyfile`.
    Caddy,
}

/// How to start a proxy process.
#[derive(Debug, Clone)]
pub enum Launcher {
    /// A local executable.
    Binary(PathBuf),

    /// A Caddy container image, run with host networking.
    Docker(String),
}

/// A proxy under test.
#[derive(Debug, Clone)]
pub struct Target {
    /// Column name in the report.
    pub name: &'static str,
    flavor: Flavor,
    http: Launcher,
    l4: Option<Launcher>,
}

impl Target {
    /// rxh, which handles HTTP and layer 4 with the same binary.
    pub fn rxh(binary: PathBuf) -> Self {
        Self {
            name: "rxh",
            flavor: Flavor::Rxh,
            http: Launcher::Binary(binary.clone()),
            l4: Some(Launcher::Binary(binary)),
        }
    }

    /// Caddy. Layer 4 needs a build with the `caddy-l4` plugin.
    pub fn caddy(http: Launcher, l4: Option<PathBuf>) -> Self {
        Self {
            name: "caddy",
            flavor: Flavor::Caddy,
            http,
            l4: l4.map(Launcher::Binary),
        }
    }

    /// Writes the config for `spec`, starts the proxy and waits until it
    /// accepts connections.
    pub async fn start(&self, spec: &ProxySpec) -> Result<Running, Problem> {
        let failed = |context: &str, err: &dyn std::fmt::Display| {
            Problem::Failed(format!("{}: {context}: {err}", self.name))
        };

        let launcher = match spec.protocol {
            Protocol::Http => &self.http,
            Protocol::Tcp => self.l4.as_ref().ok_or_else(|| {
                Problem::Unsupported(String::from(
                    "no layer 4 proxy (Caddy needs the caddy-l4 plugin, pass --caddy-l4)",
                ))
            })?,
        };

        let dir = tempfile::Builder::new()
            .prefix("rxh-bench-")
            .tempdir()
            .map_err(|err| failed("temp dir", &err))?;
        let address = free_address().map_err(|err| failed("free port", &err))?;

        let (file_name, contents) = match self.flavor {
            Flavor::Rxh => ("rxh.toml", rxh_config(spec, address)),
            Flavor::Caddy => ("Caddyfile", caddy_config(spec, address)),
        };
        let config = dir.path().join(file_name);
        fs::write(&config, contents).map_err(|err| failed("write config", &err))?;

        let log_path = dir.path().join("proxy.log");
        let log = File::create(&log_path).map_err(|err| failed("log file", &err))?;
        let log_copy = log.try_clone().map_err(|err| failed("log file", &err))?;

        let mut container = None;
        let mut command = match launcher {
            Launcher::Binary(binary) => {
                let mut command = Command::new(binary);
                match self.flavor {
                    Flavor::Rxh => {
                        command.arg("run").arg("--config").arg(&config);
                    }
                    Flavor::Caddy => {
                        command
                            .args(["run", "--adapter", "caddyfile", "--config"])
                            .arg(&config)
                            // Keep Caddy's state out of the user's home.
                            .env("XDG_DATA_HOME", dir.path())
                            .env("XDG_CONFIG_HOME", dir.path());
                    }
                }
                command
            }
            Launcher::Docker(image) => {
                let name = format!("rxh-bench-{}-{}", std::process::id(), address.port());
                let mut command = Command::new("docker");
                command
                    .args(["run", "--rm", "--name", &name, "--network", "host", "-v"])
                    .arg(format!("{}:/etc/caddy:ro", dir.path().display()))
                    .arg(image)
                    .args(["caddy", "run", "--config", "/etc/caddy/Caddyfile"])
                    .args(["--adapter", "caddyfile"]);
                container = Some(name);
                command
            }
        };

        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_copy))
            .kill_on_drop(true);
        let child = command.spawn().map_err(|err| failed("spawn", &err))?;

        let mut running = Running {
            child,
            address,
            container,
            log: log_path,
            _dir: dir,
        };
        let timeout = if running.container.is_some() { 30 } else { 10 };
        running
            .wait_ready(Duration::from_secs(timeout))
            .await
            .map_err(Problem::Failed)?;
        Ok(running)
    }
}

/// A started proxy. Dropping it kills the process (and removes the container).
pub struct Running {
    child: Child,
    address: SocketAddr,
    container: Option<String>,
    log: PathBuf,
    _dir: TempDir,
}

impl Running {
    /// Address clients connect to.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("exited with {status}: {}", log_tail(&self.log)));
            }
            if TcpStream::connect(self.address).await.is_ok() {
                // Give health checkers and pools a moment to settle.
                tokio::time::sleep(Duration::from_millis(200)).await;
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "not listening on {} after {timeout:?}: {}",
                    self.address,
                    log_tail(&self.log)
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // Killing `docker run` does not stop the container, so remove it.
        if let Some(name) = &self.container {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", name])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.start_kill();
    }
}

fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(8)..].join(" | ");
    if tail.is_empty() {
        String::from("(empty log)")
    } else {
        tail
    }
}

fn free_address() -> std::io::Result<SocketAddr> {
    TcpListener::bind("127.0.0.1:0")?.local_addr()
}

/// Protocol of the listener under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP reverse proxy.
    Http,

    /// Raw TCP proxy.
    Tcp,
}

/// Proxy setup shared by both targets, translated to rxh TOML or a Caddyfile.
#[derive(Debug, Clone)]
pub struct ProxySpec {
    /// Listener protocol.
    pub protocol: Protocol,

    /// Policy name. rxh and Caddy use the same names.
    pub policy: &'static str,

    /// Header name for the `header` policy.
    pub key: Option<&'static str>,

    /// Backend addresses and weights.
    pub backends: Vec<(SocketAddr, u32)>,

    /// Extra attempts after a failed connection.
    pub retries: u32,

    /// Active health check path, checked every second.
    pub health_uri: Option<&'static str>,

    /// Passive health checks: one failure removes a backend for 10s.
    pub passive: bool,
}

impl ProxySpec {
    /// HTTP proxy with equal weights and no health checks.
    pub fn http(policy: &'static str, backends: impl IntoIterator<Item = SocketAddr>) -> Self {
        Self {
            protocol: Protocol::Http,
            policy,
            key: None,
            backends: backends.into_iter().map(|address| (address, 1)).collect(),
            retries: 0,
            health_uri: None,
            passive: false,
        }
    }
}

/// rxh configuration, see `crates/core/src/config.rs`.
fn rxh_config(spec: &ProxySpec, listen: SocketAddr) -> String {
    let backends: Vec<String> = spec
        .backends
        .iter()
        .map(|(address, weight)| format!("{{ address = \"{address}\", weight = {weight} }}"))
        .collect();

    let mut text = String::from("[admin]\nenabled = false\n\n[upstream.bench]\n");
    let _ = writeln!(text, "policy = \"{}\"", spec.policy);
    if let Some(key) = spec.key {
        let _ = writeln!(text, "key = \"{key}\"");
    }
    if spec.retries > 0 {
        let _ = writeln!(text, "retries = {}", spec.retries);
    }
    let _ = writeln!(text, "backends = [{}]", backends.join(", "));

    if let Some(uri) = spec.health_uri {
        let _ = write!(
            text,
            "\n[upstream.bench.health]\nuri = \"{uri}\"\ninterval = \"1s\"\ntimeout = \"1s\"\n"
        );
    }
    if spec.passive {
        text.push_str("\n[upstream.bench.passive]\nfail_duration = \"10s\"\nmax_fails = 1\n");
    }

    let protocol = match spec.protocol {
        Protocol::Http => "http",
        Protocol::Tcp => "tcp",
    };
    let _ = write!(
        text,
        "\n[[server]]\nlisten = \"{listen}\"\nprotocol = \"{protocol}\"\nforward = \"@bench\"\n"
    );
    text
}

/// Caddyfile equivalent of [`rxh_config`].
fn caddy_config(spec: &ProxySpec, listen: SocketAddr) -> String {
    let mut policy = format!("lb_policy {}", spec.policy);
    if spec.policy == "weighted_round_robin" {
        for (_, weight) in &spec.backends {
            let _ = write!(policy, " {weight}");
        }
    }
    if let Some(key) = spec.key {
        let _ = write!(policy, " {key}");
    }

    if spec.protocol == Protocol::Tcp {
        let mut upstreams = String::new();
        for (address, _) in &spec.backends {
            let _ = writeln!(upstreams, "\t\t\t\t\tupstream {address}");
        }
        return format!(
            "{{\n\tadmin off\n\tpersist_config off\n\tlayer4 {{\n\t\t{listen} {{\n\t\t\troute {{\n\
             \t\t\t\tproxy {{\n\t\t\t\t\t{policy}\n{upstreams}\t\t\t\t}}\n\t\t\t}}\n\t\t}}\n\t}}\n}}\n"
        );
    }

    let upstreams: Vec<String> = spec
        .backends
        .iter()
        .map(|(address, _)| address.to_string())
        .collect();
    let mut options = format!("\t\t{policy}\n");
    if spec.retries > 0 {
        let _ = writeln!(options, "\t\tlb_retries {}", spec.retries);
    }
    if let Some(uri) = spec.health_uri {
        let _ = writeln!(
            options,
            "\t\thealth_uri {uri}\n\t\thealth_interval 1s\n\t\thealth_timeout 1s"
        );
    }
    if spec.passive {
        options.push_str("\t\tfail_duration 10s\n\t\tmax_fails 1\n");
    }

    format!(
        "{{\n\tadmin off\n\tauto_https off\n\tpersist_config off\n}}\n\n:{port} {{\n\tbind {ip}\n\
         \treverse_proxy {upstreams} {{\n{options}\t}}\n}}\n",
        port = listen.port(),
        ip = listen.ip(),
        upstreams = upstreams.join(" "),
    )
}

/// Finds the rxh binary: next to this executable, then `target/release`,
/// `target/debug` and `PATH`.
pub fn find_rxh() -> Option<PathBuf> {
    let sibling = env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("rxh")));
    let candidates = [
        sibling,
        Some(PathBuf::from("target/release/rxh")),
        Some(PathBuf::from("target/debug/rxh")),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
        .or_else(|| find_in_path("rxh"))
}

/// Looks for an executable in `PATH`.
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ProxySpec {
        let mut spec = ProxySpec::http(
            "weighted_round_robin",
            [
                "127.0.0.1:1001".parse().expect("valid"),
                "127.0.0.1:1002".parse().expect("valid"),
            ],
        );
        spec.backends[1].1 = 3;
        spec.retries = 2;
        spec.health_uri = Some("/health");
        spec.passive = true;
        spec
    }

    #[test]
    fn rxh_config_mentions_everything() {
        let text = rxh_config(&spec(), "127.0.0.1:9000".parse().expect("valid"));
        for needle in [
            "enabled = false",
            "policy = \"weighted_round_robin\"",
            "retries = 2",
            "{ address = \"127.0.0.1:1002\", weight = 3 }",
            "uri = \"/health\"",
            "fail_duration = \"10s\"",
            "listen = \"127.0.0.1:9000\"",
            "forward = \"@bench\"",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn caddyfile_mentions_everything() {
        let text = caddy_config(&spec(), "127.0.0.1:9000".parse().expect("valid"));
        for needle in [
            "admin off",
            ":9000 {",
            "bind 127.0.0.1",
            "reverse_proxy 127.0.0.1:1001 127.0.0.1:1002 {",
            "lb_policy weighted_round_robin 1 3",
            "lb_retries 2",
            "health_uri /health",
            "fail_duration 10s",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }
}
