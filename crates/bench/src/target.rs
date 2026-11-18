//! Proxies under test: how to find, configure, start and stop rxh, Caddy,
//! NGINX and Traefik.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::fs::File;
use std::net::{SocketAddr, TcpListener, UdpSocket};
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

    /// `nginx -c nginx.conf` (open source NGINX).
    Nginx,

    /// `traefik --configFile=traefik.yml` with a file provider.
    Traefik,
}

impl Flavor {
    /// Column name in the report.
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Rxh => "rxh",
            Flavor::Caddy => "caddy",
            Flavor::Nginx => "nginx",
            Flavor::Traefik => "traefik",
        }
    }

    /// Program name followed by its arguments, with the config files in `dir`.
    fn command_line(self, dir: &str) -> Vec<String> {
        let line: &[&str] = match self {
            Flavor::Rxh => &["rxh", "run", "--config", "{dir}/rxh.toml"],
            Flavor::Caddy => &[
                "caddy",
                "run",
                "--adapter",
                "caddyfile",
                "--config",
                "{dir}/Caddyfile",
            ],
            Flavor::Nginx => &["nginx", "-c", "{dir}/nginx.conf"],
            Flavor::Traefik => &["traefik", "--configFile={dir}/traefik.yml"],
        };
        line.iter().map(|arg| arg.replace("{dir}", dir)).collect()
    }

    /// Config files for `spec`, as `(file name, contents)`. `dir` is where
    /// the proxy will see them.
    fn configs(
        self,
        spec: &ProxySpec,
        listen: SocketAddr,
        dir: &str,
    ) -> Result<Vec<(&'static str, String)>, Problem> {
        let mut files = match self {
            Flavor::Rxh => vec![("rxh.toml", rxh_config(spec, listen, dir))],
            Flavor::Caddy => vec![("Caddyfile", caddy_config(spec, listen, dir))],
            Flavor::Nginx => vec![("nginx.conf", nginx_config(spec, listen, dir)?)],
            Flavor::Traefik => vec![
                ("traefik.yml", traefik_static_config(spec, listen, dir)),
                ("dynamic.yml", traefik_dynamic_config(spec, dir)?),
            ],
        };
        if let Some(tls) = &spec.tls {
            files.push(("cert.pem", tls.cert_pem.clone()));
            files.push(("key.pem", tls.key_pem.clone()));
        }
        Ok(files)
    }
}

/// Where a container sees the config directory.
const CONTAINER_DIR: &str = "/bench";

/// How to start a proxy process.
#[derive(Debug, Clone)]
pub enum Launcher {
    /// A local executable.
    Binary(PathBuf),

    /// A container, run with host networking.
    Docker {
        /// Image to run.
        image: String,

        /// Local executable mounted at `/usr/local/bin/rxh`, for images that
        /// do not ship the proxy (rxh runs in a plain base image).
        binary: Option<PathBuf>,
    },
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
    /// A proxy that handles HTTP with `http` and layer 4 with `l4`.
    pub fn new(flavor: Flavor, http: Launcher, l4: Option<Launcher>) -> Self {
        Self {
            name: flavor.name(),
            flavor,
            http,
            l4,
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
            Protocol::Tcp | Protocol::Udp => self.l4.as_ref().ok_or_else(|| {
                Problem::Unsupported(String::from(
                    "no layer 4 proxy (Caddy needs the caddy-l4 plugin, pass --caddy-l4)",
                ))
            })?,
        };

        let dir = tempfile::Builder::new()
            .prefix("rxh-bench-")
            .tempdir()
            .map_err(|err| failed("temp dir", &err))?;
        let address = free_address(spec.protocol).map_err(|err| failed("free port", &err))?;

        let host_dir = dir.path().display().to_string();
        let seen_dir = match launcher {
            Launcher::Binary(_) => host_dir.as_str(),
            Launcher::Docker { .. } => CONTAINER_DIR,
        };
        for (file_name, contents) in self.flavor.configs(spec, address, seen_dir)? {
            fs::write(dir.path().join(file_name), contents)
                .map_err(|err| failed("write config", &err))?;
        }
        let command_line = self.flavor.command_line(seen_dir);

        let log_path = dir.path().join("proxy.log");
        let log = File::create(&log_path).map_err(|err| failed("log file", &err))?;
        let log_copy = log.try_clone().map_err(|err| failed("log file", &err))?;

        let mut container = None;
        let mut command = match launcher {
            Launcher::Binary(binary) => {
                let mut command = Command::new(binary);
                command
                    .args(&command_line[1..])
                    // Keep Caddy's state out of the user's home.
                    .env("XDG_DATA_HOME", dir.path())
                    .env("XDG_CONFIG_HOME", dir.path());
                command
            }
            Launcher::Docker { image, binary } => {
                let name = format!("rxh-bench-{}-{}", std::process::id(), address.port());
                let mut command = Command::new("docker");
                command
                    .args(["run", "--rm", "--name", &name, "--network", "host", "-v"])
                    .arg(format!("{host_dir}:{CONTAINER_DIR}:ro"));
                if let Some(binary) = binary {
                    command
                        .arg("-v")
                        .arg(format!("{}:/usr/local/bin/rxh:ro", binary.display()));
                }
                command.arg(image).args(&command_line);
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
        // ponytail: fixed settle time. Traefik listens before its file
        // provider has loaded the routes; probe a route if 1s proves flaky.
        let settle = if self.flavor == Flavor::Traefik {
            1000
        } else {
            200
        };
        running
            .wait_ready(
                spec.protocol,
                Duration::from_secs(timeout),
                Duration::from_millis(settle),
            )
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

    /// Resident memory of the proxy and all its descendants (NGINX workers),
    /// in bytes. Linux only.
    pub async fn rss(&self) -> Result<u64, Problem> {
        let pid = match &self.container {
            None => self.child.id(),
            Some(name) => Command::new("docker")
                .args(["inspect", "--format", "{{.State.Pid}}", name])
                .output()
                .await
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|pid| pid.trim().parse().ok()),
        };
        let pid = pid.ok_or_else(|| Problem::Failed(String::from("proxy pid unknown")))?;
        tree_rss(pid).ok_or_else(|| {
            Problem::Unsupported(String::from("memory needs /proc (Linux) to read RSS"))
        })
    }

    async fn wait_ready(
        &mut self,
        protocol: Protocol,
        timeout: Duration,
        settle: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!("exited with {status}: {}", log_tail(&self.log)));
            }
            let ready = match protocol {
                Protocol::Http | Protocol::Tcp => TcpStream::connect(self.address).await.is_ok(),
                Protocol::Udp => udp_echoes(self.address).await,
            };
            if ready {
                // Give health checkers and pools a moment to settle.
                tokio::time::sleep(settle).await;
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

fn free_address(protocol: Protocol) -> std::io::Result<SocketAddr> {
    match protocol {
        Protocol::Http | Protocol::Tcp => TcpListener::bind("127.0.0.1:0")?.local_addr(),
        Protocol::Udp => UdpSocket::bind("127.0.0.1:0")?.local_addr(),
    }
}

/// Whether a UDP proxy at `address` forwards a datagram to an echo backend
/// and back within 200 ms.
async fn udp_echoes(address: SocketAddr) -> bool {
    let probe = async {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(address).await?;
        socket.send(b"ready?").await?;
        let mut buffer = [0; 16];
        socket.recv(&mut buffer).await
    };
    matches!(
        tokio::time::timeout(Duration::from_millis(200), probe).await,
        Ok(Ok(_))
    )
}

/// Sum of `VmRSS` over `root` and its descendants, in bytes.
fn tree_rss(root: u32) -> Option<u64> {
    let mut parents: Vec<(u32, u32)> = Vec::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let pid: Option<u32> = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok());
        let Some(pid) = pid else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The fields after the command name (which may hold spaces) are
        // state, then the parent pid.
        let parent: Option<u32> = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok());
        if let Some(parent) = parent {
            parents.push((pid, parent));
        }
    }

    let mut tree = vec![root];
    let mut index = 0;
    while let Some(pid) = tree.get(index).copied() {
        tree.extend(
            parents
                .iter()
                .filter(|(_, parent)| *parent == pid)
                .map(|(child, _)| *child),
        );
        index += 1;
    }

    let mut total = 0;
    for pid in tree {
        let status = fs::read_to_string(format!("/proc/{pid}/status")).ok();
        let kib: Option<u64> = status.as_deref().and_then(|status| {
            let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
            line.split_whitespace().nth(1)?.parse().ok()
        });
        // A process that exited meanwhile, or the root's kernel threads.
        if pid == root && kib.is_none() {
            return None;
        }
        total += kib.unwrap_or_default() * 1024;
    }
    Some(total)
}

/// Protocol of the listener under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// HTTP reverse proxy.
    Http,

    /// Raw TCP proxy.
    Tcp,

    /// UDP proxy with one session per client address.
    Udp,
}

/// A certificate and its key, PEM encoded, for TLS termination.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    /// Certificate chain.
    pub cert_pem: String,

    /// Private key.
    pub key_pem: String,
}

/// Proxy setup shared by every target, translated to each config format.
#[derive(Debug, Clone)]
pub struct ProxySpec {
    /// Listener protocol.
    pub protocol: Protocol,

    /// Policy name, as rxh and Caddy spell it. Other targets map it.
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

    /// Terminate TLS (with HTTP/2 through ALPN) with this certificate.
    pub tls: Option<TlsFiles>,

    /// Client connections the proxy must hold at once, for proxies whose
    /// default limit is lower.
    pub max_connections: Option<usize>,
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
            tls: None,
            max_connections: None,
        }
    }
}

/// rxh configuration, see `crates/core/src/config.rs`.
fn rxh_config(spec: &ProxySpec, listen: SocketAddr, dir: &str) -> String {
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
        Protocol::Udp => "udp",
    };
    let _ = write!(
        text,
        "\n[[server]]\nlisten = \"{listen}\"\nprotocol = \"{protocol}\"\nforward = \"@bench\"\n"
    );
    if spec.tls.is_some() {
        let _ = writeln!(
            text,
            "tls = {{ cert = \"{dir}/cert.pem\", key = \"{dir}/key.pem\" }}"
        );
    }
    if let Some(max) = spec.max_connections {
        let _ = writeln!(text, "max_connections = {max}");
    }
    text
}

/// Caddyfile equivalent of [`rxh_config`].
fn caddy_config(spec: &ProxySpec, listen: SocketAddr, dir: &str) -> String {
    let mut policy = format!("lb_policy {}", spec.policy);
    if spec.policy == "weighted_round_robin" {
        for (_, weight) in &spec.backends {
            let _ = write!(policy, " {weight}");
        }
    }
    if let Some(key) = spec.key {
        let _ = write!(policy, " {key}");
    }

    if spec.protocol != Protocol::Http {
        let network = if spec.protocol == Protocol::Udp {
            "udp/"
        } else {
            ""
        };
        let mut upstreams = String::new();
        for (address, _) in &spec.backends {
            let _ = writeln!(upstreams, "\t\t\t\t\tupstream {network}{address}");
        }
        return format!(
            "{{\n\tadmin off\n\tpersist_config off\n\tlayer4 {{\n\t\t{network}{listen} {{\n\
             \t\t\troute {{\n\t\t\t\tproxy {{\n\t\t\t\t\t{policy}\n{upstreams}\t\t\t\t}}\n\
             \t\t\t}}\n\t\t}}\n\t}}\n}}\n"
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

    let tls = if spec.tls.is_some() {
        format!("\ttls {dir}/cert.pem {dir}/key.pem\n")
    } else {
        String::new()
    };
    format!(
        "{{\n\tadmin off\n\tauto_https off\n\tpersist_config off\n}}\n\n:{port} {{\n\tbind {ip}\n\
         {tls}\treverse_proxy {upstreams} {{\n{options}\t}}\n}}\n",
        port = listen.port(),
        ip = listen.ip(),
        upstreams = upstreams.join(" "),
    )
}

/// `nginx.conf` equivalent of [`rxh_config`], limited to open source NGINX.
fn nginx_config(spec: &ProxySpec, listen: SocketAddr, dir: &str) -> Result<String, Problem> {
    let method = match spec.policy {
        "round_robin" | "weighted_round_robin" => String::new(),
        "least_conn" => String::from("least_conn;"),
        // `ip_hash` only looks at the first three octets, so every loopback
        // client would land on one backend.
        "ip_hash" => String::from("hash $remote_addr consistent;"),
        "uri_hash" => String::from("hash $request_uri consistent;"),
        "header" => format!(
            "hash $http_{} consistent;",
            spec.key
                .unwrap_or_default()
                .to_lowercase()
                .replace('-', "_")
        ),
        "cookie" => {
            return Err(Problem::Unsupported(String::from(
                "sticky cookies need NGINX Plus",
            )));
        }
        other => return Err(Problem::Unsupported(format!("no `{other}` policy"))),
    };
    if spec.health_uri.is_some() {
        return Err(Problem::Unsupported(String::from(
            "active health checks need NGINX Plus",
        )));
    }

    // NGINX marks backends as failed by default; switch that off unless asked.
    let max_fails = u8::from(spec.passive);
    let mut upstream = format!("upstream bench {{\n zone bench 64k;\n {method}\n");
    for (address, weight) in &spec.backends {
        let _ = writeln!(
            upstream,
            " server {address} weight={weight} max_fails={max_fails} fail_timeout=10s;"
        );
    }

    let body = match spec.protocol {
        Protocol::Tcp => format!(
            "stream {{\n{upstream}}}\nserver {{\n listen {listen};\n proxy_pass bench;\n}}\n}}\n"
        ),
        Protocol::Udp => format!(
            "stream {{\n{upstream}}}\nserver {{\n listen {listen} udp;\n proxy_pass bench;\n \
             proxy_timeout 10s;\n}}\n}}\n"
        ),
        Protocol::Http => {
            let tls = if spec.tls.is_some() {
                format!(
                    " ssl;\n http2 on;\n ssl_certificate {dir}/cert.pem;\n \
                     ssl_certificate_key {dir}/key.pem"
                )
            } else {
                String::new()
            };
            // Bodies of any size, no GOAWAY after 1000 requests on a shared
            // HTTP/2 connection (streams in flight would fail), and the
            // `Upgrade` and `Connection` headers WebSockets need while other
            // requests keep upstream keep-alive.
            format!(
                "http {{\naccess_log off;\nclient_max_body_size 0;\nkeepalive_requests 1000000;\n\
                 map $http_upgrade $connection_upgrade {{\n default upgrade;\n '' '';\n}}\n\
                 {upstream} keepalive 64;\n}}\nserver {{\n listen {listen}{tls};\n \
                 location / {{\n  proxy_pass http://bench;\n  proxy_http_version 1.1;\n  \
                 proxy_set_header Upgrade $http_upgrade;\n  \
                 proxy_set_header Connection $connection_upgrade;\n  proxy_set_header Host $host;\n  \
                 proxy_next_upstream error timeout;\n  proxy_next_upstream_tries {tries};\n }}\n}}\n}}\n",
                tries = spec.retries + 1
            )
        }
    };
    // Each proxied connection takes two slots (client and upstream).
    let connections = spec.max_connections.unwrap_or_default().max(2048) * 2;
    Ok(format!(
        "worker_processes auto;\nworker_rlimit_nofile {files};\ndaemon off;\n\
         pid /tmp/rxh-bench-nginx.pid;\nerror_log stderr error;\n\
         events {{\n worker_connections {connections};\n}}\n{body}",
        files = connections * 2
    ))
}

/// Traefik static configuration: one entry point and the file provider.
fn traefik_static_config(spec: &ProxySpec, listen: SocketAddr, dir: &str) -> String {
    let network = if spec.protocol == Protocol::Udp {
        "/udp"
    } else {
        ""
    };
    format!(
        "global:\n  checkNewVersion: false\n  sendAnonymousUsage: false\nentryPoints:\n  bench:\n    \
         address: \"{listen}{network}\"\nproviders:\n  file:\n    filename: {dir}/dynamic.yml\n\
         log:\n  level: ERROR\n"
    )
}

/// Traefik dynamic configuration equivalent of [`rxh_config`].
fn traefik_dynamic_config(spec: &ProxySpec, dir: &str) -> Result<String, Problem> {
    if spec.protocol == Protocol::Udp {
        let mut text = String::from(
            "udp:\n  routers:\n    bench:\n      entryPoints: [bench]\n      service: bench\n  \
             services:\n    bench:\n      loadBalancer:\n        servers:\n",
        );
        for (address, _) in &spec.backends {
            let _ = writeln!(text, "          - address: \"{address}\"");
        }
        return Ok(text);
    }
    if spec.protocol == Protocol::Tcp {
        let mut text = String::from(
            "tcp:\n  routers:\n    bench:\n      rule: \"HostSNI(`*`)\"\n      \
             entryPoints: [bench]\n      service: bench\n  services:\n    bench:\n      \
             loadBalancer:\n        servers:\n",
        );
        for (address, _) in &spec.backends {
            let _ = writeln!(text, "          - address: \"{address}\"");
        }
        return Ok(text);
    }

    let mut balancer = String::new();
    match spec.policy {
        "round_robin" | "weighted_round_robin" => {}
        // Power of two choices on connections in flight: Traefik's closest
        // relative of `least_conn`.
        "least_conn" => balancer.push_str("        strategy: p2c\n"),
        // Rendezvous hashing on the client IP.
        "ip_hash" => balancer.push_str("        strategy: hrw\n"),
        "cookie" => balancer.push_str("        sticky:\n          cookie:\n            name: lb\n"),
        other => {
            return Err(Problem::Unsupported(format!(
                "no `{other}` policy (Traefik hashes only the client IP)"
            )));
        }
    }
    if let Some(uri) = spec.health_uri {
        let _ = write!(
            balancer,
            "        healthCheck:\n          path: {uri}\n          interval: 1s\n          \
             timeout: 1s\n"
        );
    }
    if spec.passive {
        balancer.push_str(
            "        passiveHealthCheck:\n          failureWindow: 10s\n          \
             maxFailedAttempts: 1\n",
        );
    }
    balancer.push_str("        servers:\n");
    for (address, weight) in &spec.backends {
        let _ = writeln!(
            balancer,
            "          - url: \"http://{address}\"\n            weight: {weight}"
        );
    }

    let mut router = String::from(
        "http:\n  routers:\n    bench:\n      rule: \"PathPrefix(`/`)\"\n      \
         entryPoints: [bench]\n      service: bench\n",
    );
    let mut tls = String::new();
    if spec.tls.is_some() {
        router.push_str("      tls: {}\n");
        tls = format!(
            "tls:\n  stores:\n    default:\n      defaultCertificate:\n        \
             certFile: {dir}/cert.pem\n        keyFile: {dir}/key.pem\n"
        );
    }
    if spec.retries > 0 {
        let _ = write!(
            router,
            "      middlewares: [retry]\n  middlewares:\n    retry:\n      retry:\n        \
             attempts: {}\n",
            spec.retries + 1
        );
    }
    Ok(format!(
        "{router}  services:\n    bench:\n      loadBalancer:\n{balancer}{tls}"
    ))
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
        let text = rxh_config(&spec(), "127.0.0.1:9000".parse().expect("valid"), "/b");
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
        let text = caddy_config(&spec(), "127.0.0.1:9000".parse().expect("valid"), "/b");
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
    #[test]
    fn nginx_config_mentions_everything() {
        let mut spec = spec();
        spec.health_uri = None;
        let text = nginx_config(&spec, "127.0.0.1:9000".parse().expect("valid"), "/b")
            .unwrap_or_else(|problem| panic!("config should render: {problem:?}"));
        for needle in [
            "listen 127.0.0.1:9000;",
            "server 127.0.0.1:1002 weight=3 max_fails=1 fail_timeout=10s;",
            "proxy_next_upstream_tries 3;",
            "keepalive 64;",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn nginx_rejects_plus_only_features() {
        let listen = "127.0.0.1:9000".parse().expect("valid");
        assert!(
            matches!(
                nginx_config(&spec(), listen, "/b"),
                Err(Problem::Unsupported(_))
            ),
            "active health checks are NGINX Plus only"
        );
        let cookie = ProxySpec::http("cookie", []);
        assert!(
            matches!(
                nginx_config(&cookie, listen, "/b"),
                Err(Problem::Unsupported(_))
            ),
            "sticky cookies are NGINX Plus only"
        );
    }

    #[test]
    fn traefik_config_mentions_everything() {
        let text = traefik_dynamic_config(&spec(), "/b")
            .unwrap_or_else(|problem| panic!("config should render: {problem:?}"));
        for needle in [
            "- url: \"http://127.0.0.1:1002\"\n            weight: 3",
            "attempts: 3",
            "path: /health",
            "maxFailedAttempts: 1",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        let hash = ProxySpec::http("uri_hash", []);
        assert!(
            matches!(
                traefik_dynamic_config(&hash, "/b"),
                Err(Problem::Unsupported(_))
            ),
            "Traefik has no URI hash"
        );
    }

    fn assert_contains(text: &str, needles: &[&str]) {
        for needle in needles {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn tls_and_connection_limit_render_for_every_proxy() {
        let listen: SocketAddr = "127.0.0.1:9000".parse().expect("valid");
        let mut spec = ProxySpec::http("round_robin", ["127.0.0.1:1001".parse().expect("valid")]);
        spec.tls = Some(TlsFiles {
            cert_pem: String::from("CERT"),
            key_pem: String::from("KEY"),
        });
        spec.max_connections = Some(5000);

        assert_contains(
            &rxh_config(&spec, listen, "/b"),
            &[
                "tls = { cert = \"/b/cert.pem\", key = \"/b/key.pem\" }",
                "max_connections = 5000",
            ],
        );
        assert_contains(
            &caddy_config(&spec, listen, "/b"),
            &["tls /b/cert.pem /b/key.pem"],
        );
        let nginx = nginx_config(&spec, listen, "/b").unwrap();
        assert_contains(
            &nginx,
            &[
                "listen 127.0.0.1:9000 ssl;",
                "http2 on;",
                "ssl_certificate /b/cert.pem;",
                "ssl_certificate_key /b/key.pem;",
                "worker_connections 10000;",
                "client_max_body_size 0;",
                "proxy_set_header Upgrade $http_upgrade;",
            ],
        );
        assert_contains(
            &traefik_dynamic_config(&spec, "/b").unwrap(),
            &[
                "      tls: {}",
                "certFile: /b/cert.pem",
                "keyFile: /b/key.pem",
            ],
        );

        let files = Flavor::Rxh.configs(&spec, listen, "/b").unwrap();
        let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["rxh.toml", "cert.pem", "key.pem"]);
        assert_eq!(files[1].1, "CERT");
    }

    #[test]
    fn udp_renders_for_every_proxy() {
        let listen: SocketAddr = "127.0.0.1:9000".parse().expect("valid");
        let mut spec = ProxySpec::http("round_robin", ["127.0.0.1:1001".parse().expect("valid")]);
        spec.protocol = Protocol::Udp;

        assert_contains(&rxh_config(&spec, listen, "/b"), &["protocol = \"udp\""]);
        assert_contains(
            &caddy_config(&spec, listen, "/b"),
            &["udp/127.0.0.1:9000 {", "upstream udp/127.0.0.1:1001"],
        );
        assert_contains(
            &nginx_config(&spec, listen, "/b").unwrap(),
            &["stream {", "listen 127.0.0.1:9000 udp;"],
        );
        assert_contains(
            &traefik_static_config(&spec, listen, "/b"),
            &["address: \"127.0.0.1:9000/udp\""],
        );
        assert_contains(
            &traefik_dynamic_config(&spec, "/b").unwrap(),
            &["udp:\n  routers:", "- address: \"127.0.0.1:1001\""],
        );
    }

    #[test]
    fn rss_covers_children_and_needs_a_live_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let alone = tree_rss(child.id()).unwrap();
        let with_child = tree_rss(std::process::id()).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();

        assert!(alone > 0, "a running process has resident pages");
        assert!(
            with_child > alone,
            "the parent's own pages come on top of its child's"
        );
        assert_eq!(tree_rss(u32::MAX), None, "no such process");
    }

    #[tokio::test]
    async fn udp_readiness_needs_an_echo() {
        let silent = UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(!udp_echoes(silent.local_addr().unwrap()).await);

        let echo = crate::backend::UdpEchoBackend::spawn().await.unwrap();
        assert!(udp_echoes(echo.address()).await);
    }
}
