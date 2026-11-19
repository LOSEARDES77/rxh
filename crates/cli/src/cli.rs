//! Command line definition.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use rxh_core::config::AdminListen;

/// RXH: reverse proxy and layer 4 load balancer (HTTP, TCP, UDP) with a
/// daemon and live upstream management.
#[derive(Debug, Parser)]
#[command(name = "rxh", version = rxh_server::VERSION, about, long_about = None)]
pub(crate) struct Cli {
    /// Configuration file. Commands that edit the configuration use the file
    /// of the running daemon when this is not given.
    #[arg(short, long, global = true, value_name = "FILE", env = "RXH_CONFIG")]
    pub(crate) config: Option<PathBuf>,

    /// Admin API address of the running daemon, `ip:port` or
    /// `unix//path/to/admin.sock`. Defaults to `[admin] listen` from the
    /// configuration file, or 127.0.0.1:2019.
    #[arg(long, global = true, value_name = "ADDR", env = "RXH_ADMIN")]
    pub(crate) admin: Option<AdminListen>,

    /// Token of the admin API. Defaults to `[admin] token` from the
    /// configuration file when the address is the one of that file.
    #[arg(
        long,
        global = true,
        value_name = "TOKEN",
        env = "RXH_ADMIN_TOKEN",
        hide_env_values = true
    )]
    pub(crate) admin_token: Option<String>,

    /// PID file of a daemon started with `rxh start`.
    #[arg(
        long,
        global = true,
        value_name = "FILE",
        default_value = "/tmp/rxh/rxh.pid"
    )]
    pub(crate) pid_file: PathBuf,

    #[command(subcommand)]
    pub(crate) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Run in the foreground (default).
    Run,

    /// Start as a background daemon.
    Start {
        /// Log file. Defaults to the PID file with a `.log` extension.
        #[arg(long, value_name = "FILE")]
        log_file: Option<PathBuf>,
    },

    /// Stop the daemon gracefully.
    Stop,

    /// Reload the configuration file without dropping connections.
    Reload,

    /// Show listeners, pools and backend health.
    Status {
        /// Print the raw JSON from the admin API.
        #[arg(long)]
        json: bool,
    },

    /// Check the configuration file and exit.
    Validate,

    /// Manage backends of a load balancing pool.
    #[command(subcommand, visible_alias = "peer")]
    Upstream(UpstreamCommand),

    /// Manage load balancing pools (`[upstream.NAME]` tables).
    #[command(subcommand, visible_alias = "group")]
    Pool(PoolCommand),

    /// Manage services (`[[server]]` listeners).
    #[command(subcommand)]
    Service(ServiceCommand),

    /// Certificates served by the running daemon.
    #[command(subcommand, visible_alias = "certs")]
    Cert(CertCommand),

    /// List load balancing policies.
    Policies,

    /// Hash the password read from standard input, for `basic_auth` users.
    HashPassword,

    /// Print the shell completion script for SHELL to standard output.
    Completions {
        /// Shell to complete for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Print the rxh(1) man page, or write one page per command to a
    /// directory.
    Man {
        /// Write `rxh.1` and `rxh-COMMAND.1` pages into this directory
        /// instead of printing `rxh.1` to standard output.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
    },

    /// Show the effective configuration, with includes merged.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Serve a directory over HTTP right now, without a configuration file.
    FileServer {
        /// Address to listen on.
        #[arg(short, long, default_value = "127.0.0.1:8080", value_name = "ADDR")]
        listen: SocketAddr,

        /// Directory to serve.
        #[arg(short, long, default_value = ".", value_name = "DIR")]
        root: PathBuf,

        /// Do not list directories that have no index file.
        #[arg(long)]
        no_browse: bool,

        /// Compress responses with zstd and gzip on the fly.
        #[arg(long)]
        encode: bool,
    },

    /// Proxy an address to backends right now, without a configuration file.
    ReverseProxy {
        /// Address to listen on.
        #[arg(short, long, value_name = "ADDR")]
        from: SocketAddr,

        /// Backend `host:port`. Repeatable, or several after one `--to`.
        #[arg(short, long, required = true, num_args = 1.., value_name = "BACKEND")]
        to: Vec<String>,

        /// Load balancing policy (see `rxh policies`).
        #[arg(short, long, default_value = "weighted_round_robin")]
        policy: String,

        /// Protocol.
        #[arg(long, value_enum, default_value = "http")]
        protocol: ServiceProtocol,

        /// Only answer requests for this host name (http only).
        #[arg(long, value_name = "NAME")]
        host: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommand {
    /// Print the effective configuration: the running daemon's when it is
    /// reachable, else the files.
    Show {
        /// Print JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum CertCommand {
    /// List the certificates of every TLS listener, soonest expiry first.
    #[command(visible_alias = "ls")]
    List {
        /// Print the raw JSON from the admin API.
        #[arg(long)]
        json: bool,
    },

    /// Read every certificate file again now. The daemon also does it by
    /// itself within seconds of a change.
    Reload,
}

#[derive(Debug, Subcommand)]
pub(crate) enum UpstreamCommand {
    /// List backends with their live health.
    #[command(visible_alias = "ls")]
    List {
        /// Only this pool.
        pool: Option<String>,
    },

    /// Add a backend to a pool, live and in the configuration file.
    Add {
        /// Pool name.
        pool: String,

        /// Backend `host:port`. Port ranges like `10.0.0.1:8001-8004` work.
        address: String,

        /// Weight for `weighted_round_robin`.
        #[arg(short, long, default_value_t = 1)]
        weight: u32,

        #[command(flatten)]
        persistence: Persistence,
    },

    /// Remove a backend from a pool. Open connections are not interrupted.
    #[command(visible_alias = "rm")]
    Remove {
        /// Pool name.
        pool: String,

        /// Backend `host:port`.
        address: String,

        #[command(flatten)]
        persistence: Persistence,
    },
}

#[derive(Debug, Clone, Copy, Args)]
pub(crate) struct Persistence {
    /// Only change the running daemon, not the configuration file. The
    /// change is lost on the next reload.
    #[arg(long)]
    pub(crate) no_save: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PoolCommand {
    /// List pools.
    #[command(visible_alias = "ls")]
    List,

    /// Create a pool and reload the daemon.
    Add {
        /// Pool name, referenced from services as `@NAME`.
        name: String,

        /// Load balancing policy (see `rxh policies`).
        #[arg(short, long, default_value = "weighted_round_robin")]
        policy: String,

        /// Backend `host:port`, repeatable.
        #[arg(short, long = "backend", value_name = "ADDR")]
        backends: Vec<String>,

        /// Header, query parameter or cookie name for hash policies.
        #[arg(long)]
        key: Option<String>,

        /// Retries after a failed connection.
        #[arg(long)]
        retries: Option<u32>,

        /// Path for active HTTP health checks. Without it, `--health-interval`
        /// enables TCP connect checks.
        #[arg(long, value_name = "URI")]
        health_uri: Option<String>,

        /// Interval of active health checks, like `10s`.
        #[arg(long, value_name = "DURATION")]
        health_interval: Option<String>,

        /// Enable passive health checks remembering failures this long, like `30s`.
        #[arg(long, value_name = "DURATION")]
        fail_duration: Option<String>,

        /// File to write to instead of the main configuration file. Created
        /// if missing and added to `include` of the main file. A relative
        /// path is relative to the directory of the main file.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },

    /// Delete a pool and reload the daemon.
    #[command(visible_alias = "rm")]
    Remove {
        /// Pool name.
        name: String,
    },
}

/// Protocol of a new service.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ServiceProtocol {
    Http,
    Tcp,
    Udp,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// List services from the configuration file.
    #[command(visible_alias = "ls")]
    List,

    /// Add a listener and reload the daemon.
    Add {
        /// Address to listen on, like `0.0.0.0:8080`. Repeatable.
        #[arg(short, long, required = true, value_name = "ADDR")]
        listen: Vec<SocketAddr>,

        /// Protocol.
        #[arg(short, long, value_enum, default_value = "http")]
        protocol: ServiceProtocol,

        /// Forward target: `@pool` or backend addresses. Repeatable.
        #[arg(short, long, value_name = "TARGET", conflicts_with = "serve")]
        to: Vec<String>,

        /// Serve static files from this directory (http only).
        #[arg(short, long, value_name = "DIR")]
        serve: Option<PathBuf>,

        /// Service name, shown in logs and used by `service remove`.
        #[arg(short, long)]
        name: Option<String>,

        /// Path prefix (http only).
        #[arg(short, long)]
        uri: Option<String>,

        /// Host name or `*.domain` to answer to (http only). Repeatable.
        #[arg(long, value_name = "NAME")]
        host: Vec<String>,

        /// File to write to instead of the main configuration file. Created
        /// if missing and added to `include` of the main file. A relative
        /// path is relative to the directory of the main file.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },

    /// Remove a service by name or listen address and reload the daemon.
    #[command(visible_alias = "rm")]
    Remove {
        /// Service name or one of its listen addresses.
        service: String,
    },
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_upstream_add() {
        let cli = Cli::parse_from(["rxh", "upstream", "add", "api", "10.0.0.1:80", "-w", "3"]);
        let Some(Command::Upstream(UpstreamCommand::Add { pool, weight, .. })) = cli.command else {
            panic!("unexpected parse: {cli:?}");
        };
        assert_eq!(pool, "api");
        assert_eq!(weight, 3);
    }

    #[test]
    fn parses_quick_commands() {
        let cli = Cli::parse_from(["rxh", "file-server", "--encode", "-r", "/srv"]);
        let Some(Command::FileServer {
            listen,
            root,
            encode,
            ..
        }) = cli.command
        else {
            panic!("unexpected parse: {cli:?}");
        };
        assert_eq!(listen.to_string(), "127.0.0.1:8080");
        assert_eq!(root, PathBuf::from("/srv"));
        assert!(encode);

        let cli = Cli::parse_from([
            "rxh",
            "reverse-proxy",
            "--from",
            "0.0.0.0:80",
            "--to",
            "a:1",
            "b:2",
        ]);
        let Some(Command::ReverseProxy { to, .. }) = cli.command else {
            panic!("unexpected parse: {cli:?}");
        };
        assert_eq!(to, ["a:1", "b:2"]);
    }

    #[test]
    fn parses_cert_commands() {
        let cli = Cli::parse_from(["rxh", "cert", "ls", "--json"]);
        assert!(matches!(
            cli.command,
            Some(Command::Cert(CertCommand::List { json: true }))
        ));
        let cli = Cli::parse_from(["rxh", "certs", "reload"]);
        assert!(matches!(
            cli.command,
            Some(Command::Cert(CertCommand::Reload))
        ));
    }

    #[test]
    fn no_command_means_run() {
        let cli = Cli::parse_from(["rxh", "-c", "custom.toml"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("custom.toml")));
    }
}
