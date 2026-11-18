//! Command line definition.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// RXH: reverse proxy and layer 4 load balancer (HTTP, TCP, UDP) with a
/// daemon and live upstream management.
#[derive(Debug, Parser)]
#[command(name = "rxh", version = rxh_server::VERSION, about, long_about = None)]
pub(crate) struct Cli {
    /// Configuration file. Commands that edit the configuration use the file
    /// of the running daemon when this is not given.
    #[arg(short, long, global = true, value_name = "FILE", env = "RXH_CONFIG")]
    pub(crate) config: Option<PathBuf>,

    /// Admin API address of the running daemon. Defaults to `[admin] listen`
    /// from the configuration file, or 127.0.0.1:2019.
    #[arg(long, global = true, value_name = "ADDR", env = "RXH_ADMIN")]
    pub(crate) admin: Option<SocketAddr>,

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

    /// List load balancing policies.
    Policies,
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
    fn no_command_means_run() {
        let cli = Cli::parse_from(["rxh", "-c", "custom.toml"]);
        assert!(cli.command.is_none());
        assert_eq!(cli.config, Some(PathBuf::from("custom.toml")));
    }
}
