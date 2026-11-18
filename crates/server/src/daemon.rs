//! Long running process: loads the configuration, runs it, serves the admin
//! API and reloads without dropping connections.
//!
//! A reload builds a complete new generation (pools, listeners, health
//! checks) next to the running one. Listeners use `SO_REUSEPORT`, so the new
//! generation binds the same ports, then the old one stops accepting and
//! drains its connections in the background. If the configuration is invalid
//! the old generation keeps running untouched.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Instant;

use rxh_core::config::{Config, IncludedConfig};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::admin::{self, AdminState, Control, Current};
use crate::error::Error;
use crate::include;
use crate::master::{Master, ServerInfo};
use crate::registry::Registry;

fn read_file(path: &Path) -> Result<String, Error> {
    Ok(std::fs::read_to_string(path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?)
}

/// Reads the main file and every file it includes, merged but not validated.
fn read_merged(path: &Path) -> Result<(Config, Vec<PathBuf>), Error> {
    let mut config = Config::parse(&read_file(path)?)?;
    let included = include::expand(path, &config.include)?;
    for file in &included {
        let part = IncludedConfig::from_toml(&read_file(file)?).map_err(|err| {
            rxh_core::config::ConfigError::Invalid(format!("{}: {err}", file.display()))
        })?;
        config.merge(part, &file.display().to_string())?;
    }
    config.include.clear();
    let mut files = vec![path.to_owned()];
    files.extend(included);
    Ok((config, files))
}

/// Reads and validates a configuration file, following its `include` list.
/// Servers keep the order of the main file first, then the included files
/// sorted by path. The returned `include` list is empty, it is merged.
pub fn load_config(path: &Path) -> Result<Config, Error> {
    let (config, _) = read_merged(path)?;
    config.validate()?;
    Ok(config)
}

/// The main configuration file followed by the files it includes, sorted by
/// path. The files are parsed but the result is not validated.
pub fn config_files(path: &Path) -> Result<Vec<PathBuf>, Error> {
    Ok(read_merged(path)?.1)
}

/// Daemon bound to a configuration file, or to a configuration without one.
#[derive(Debug, Clone)]
pub struct Daemon {
    source: Source,
}

#[derive(Debug, Clone)]
enum Source {
    File(PathBuf),
    Memory(Config),
}

/// One running configuration.
struct Generation {
    config: Arc<Config>,
    registry: Arc<Registry>,
    servers: Vec<ServerInfo>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), Error>>,
    health_checks: JoinSet<()>,
}

impl Generation {
    fn start(config: Config, previous: Option<&Registry>) -> Result<Self, Error> {
        let registry = Arc::new(Registry::build(&config)?);
        if let Some(previous) = previous {
            registry.inherit_dynamic(previous);
        }
        let (stop, stopped) = oneshot::channel::<()>();
        let master = Master::init(&config, Arc::clone(&registry))?.shutdown_on(async move {
            let _ = stopped.await;
        });
        let servers = master.servers();
        let health_checks = registry.start_background_tasks();
        let task = tokio::task::spawn(master.run());

        Ok(Self {
            config: Arc::new(config),
            registry,
            servers,
            stop: Some(stop),
            task,
            health_checks,
        })
    }

    fn current(&self) -> Current {
        Current {
            config: Arc::clone(&self.config),
            registry: Arc::clone(&self.registry),
            servers: self.servers.clone(),
        }
    }

    /// Graceful stop, waiting for every connection.
    async fn stop(mut self) -> Result<(), Error> {
        self.health_checks.abort_all();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match self.task.await {
            Ok(result) => result,
            Err(join_error) => Err(Error::Io(io::Error::other(join_error))),
        }
    }
}

enum Event {
    Reload,
    Stop,
}

impl Daemon {
    /// Daemon for the configuration file at `config_path`.
    #[must_use]
    pub fn new(config_path: impl Into<PathBuf>) -> Self {
        Self {
            source: Source::File(config_path.into()),
        }
    }

    /// Daemon for a configuration that has no file. Reload requests fail
    /// with "no configuration file to reload".
    #[must_use]
    pub fn from_config(config: Config) -> Self {
        Self {
            source: Source::Memory(config),
        }
    }

    /// Runs until `SIGTERM`, `SIGINT` or `POST /stop`. `SIGHUP` and
    /// `POST /reload` reload the configuration file.
    pub async fn run(self) -> Result<(), Error> {
        let config = match &self.source {
            Source::Memory(config) => {
                config.validate()?;
                config.clone()
            }
            Source::File(path) => load_config(path)?,
        };
        let admin_settings = config.admin.clone();
        let mut generation = Generation::start(config, None)?;

        let (control, mut controls) = mpsc::channel(8);
        let admin_state = Arc::new(AdminState {
            started: Instant::now(),
            config_path: self
                .config_path()
                .and_then(|path| std::fs::canonicalize(path).ok()),
            listen: admin_settings.listen,
            current: RwLock::new(generation.current()),
            control,
        });

        let _admin = if admin_settings.enabled {
            let listener = tokio::net::TcpListener::bind(admin_settings.listen).await.map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "admin API on {}: {err} (is another rxh running? set [admin] listen or enabled = false)",
                        admin_settings.listen
                    ),
                )
            })?;
            println!("Admin => API listening on http://{}", admin_settings.listen);
            Some(AbortOnDrop(tokio::task::spawn(admin::serve(
                listener,
                Arc::clone(&admin_state),
            ))))
        } else {
            None
        };

        let mut signals = Signals::new()?;

        loop {
            let event = tokio::select! {
                event = signals.next() => event,
                Some(control) = controls.recv() => match control {
                    Control::Stop => Event::Stop,
                    Control::Reload(reply) => {
                        let outcome = self.reload(&mut generation, &admin_state).await;
                        let _ = reply.send(outcome);
                        continue;
                    }
                },
                result = &mut generation.task => {
                    return match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(err)) => Err(err),
                        Err(join_error) => Err(Error::Io(io::Error::other(join_error))),
                    };
                }
            };

            match event {
                Event::Stop => break,
                Event::Reload => {
                    if let Err(message) = self.reload(&mut generation, &admin_state).await {
                        eprintln!(
                            "Main => Reload failed, keeping the running configuration: {message}"
                        );
                    }
                }
            }
        }

        println!("Main => Shutting down");
        generation.stop().await
    }

    fn config_path(&self) -> Option<&Path> {
        match &self.source {
            Source::File(path) => Some(path),
            Source::Memory(_) => None,
        }
    }

    async fn reload(&self, generation: &mut Generation, admin: &AdminState) -> Result<(), String> {
        let path = self
            .config_path()
            .ok_or("no configuration file to reload")?;
        println!("Main => Reloading {}", path.display());
        let config = load_config(path).map_err(|err| err.to_string())?;

        if config.admin.listen != admin.listen {
            eprintln!("Main => [admin] changes take effect after a restart");
        }

        let previous_registry = Arc::clone(&generation.registry);
        let next = match Generation::start(config.clone(), Some(&previous_registry)) {
            Ok(next) => next,
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::AddrInUse => {
                // UDP sockets and platforms without SO_REUSEPORT cannot bind
                // twice: stop first, then start, restoring the old
                // configuration if the new one still fails.
                let previous = Config::clone(&generation.config);
                let old = std::mem::replace(generation, placeholder()?);
                old.stop().await.map_err(|err| err.to_string())?;
                match Generation::start(config, Some(&previous_registry)) {
                    Ok(next) => next,
                    Err(err) => {
                        *generation = Generation::start(previous, Some(&previous_registry))
                            .map_err(|err| err.to_string())?;
                        *admin
                            .current
                            .write()
                            .unwrap_or_else(PoisonError::into_inner) = generation.current();
                        return Err(err.to_string());
                    }
                }
            }
            Err(err) => return Err(err.to_string()),
        };

        let old = std::mem::replace(generation, next);
        *admin
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner) = generation.current();
        tokio::task::spawn(async move {
            if let Err(err) = old.stop().await {
                eprintln!("Main => Previous configuration stopped with error: {err}");
            }
        });

        println!("Main => Reload complete");
        Ok(())
    }
}

/// Empty generation used while swapping.
fn placeholder() -> Result<Generation, String> {
    Generation::start(Config::default(), None).map_err(|err| err.to_string())
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(unix)]
struct Signals {
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    async fn next(&mut self) -> Event {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Event::Stop,
            _ = self.terminate.recv() => Event::Stop,
            _ = self.hangup.recv() => Event::Reload,
        }
    }
}

#[cfg(not(unix))]
struct Signals;

#[cfg(not(unix))]
impl Signals {
    fn new() -> io::Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> Event {
        let _ = tokio::signal::ctrl_c().await;
        Event::Stop
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    }

    const MAIN: &str = r#"
include = ["conf.d/*.toml", "extra.toml"]

[[server]]
name = "main"
listen = "127.0.0.1:0"
forward = "@api"
"#;

    fn server(name: &str) -> String {
        format!("[[server]]\nname = \"{name}\"\nlisten = \"127.0.0.1:0\"\nforward = \"@api\"\n")
    }

    #[test]
    fn includes_merge_in_order_across_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "rxh.toml", MAIN);
        write(
            dir.path(),
            "conf.d/b.toml",
            &format!(
                "[upstream.api]\nbackends = [\"127.0.0.1:1\"]\n{}",
                server("b")
            ),
        );
        write(dir.path(), "conf.d/a.toml", &server("a"));
        write(dir.path(), "extra.toml", &server("extra"));

        let main = dir.path().join("rxh.toml");
        let config = load_config(&main).expect("loads");
        let names: Vec<_> = config
            .servers
            .iter()
            .map(|server| server.name.as_deref().expect("named"))
            .collect();
        assert_eq!(names, ["main", "a", "b", "extra"]);
        assert!(config.upstreams.contains_key("api"));
        assert!(config.include.is_empty());
        assert_eq!(config_files(&main).expect("files").len(), 4);
    }

    #[test]
    fn shipped_include_example_loads() {
        let main = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/includes/rxh.toml");
        let config = load_config(&main).expect("example loads");
        assert!(config.upstreams.contains_key("api"));
        assert_eq!(config.servers.len(), 1);
    }

    #[test]
    fn include_rules_are_enforced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("rxh.toml");
        write(
            dir.path(),
            "rxh.toml",
            "include = [\"x.toml\"]\n[upstream.api]\nbackends = [\"127.0.0.1:1\"]\n",
        );

        write(dir.path(), "x.toml", "[admin]\nenabled = false\n");
        let err = load_config(&main)
            .expect_err("admin not allowed")
            .to_string();
        assert!(err.contains("x.toml") && err.contains("admin"), "{err}");

        write(
            dir.path(),
            "x.toml",
            "[upstream.api]\nbackends = [\"127.0.0.1:2\"]\n",
        );
        let err = load_config(&main).expect_err("duplicate pool").to_string();
        assert!(err.contains("pool api"), "{err}");

        write(dir.path(), "x.toml", "");
        fs::write(&main, "include = [\"missing.toml\"]\n").expect("write");
        assert!(load_config(&main).is_err());
    }

    #[tokio::test]
    async fn daemon_without_a_file_cannot_reload() {
        let config = Config::from_toml("[admin]\nenabled = false\n").expect("valid");
        let daemon = Daemon::from_config(config);
        assert!(daemon.config_path().is_none());
        let mut generation = placeholder().expect("generation");
        let admin = {
            let (control, _) = mpsc::channel(1);
            AdminState {
                started: Instant::now(),
                config_path: None,
                listen: "127.0.0.1:0".parse().expect("address"),
                current: RwLock::new(generation.current()),
                control,
            }
        };
        let err = daemon
            .reload(&mut generation, &admin)
            .await
            .expect_err("no file");
        assert_eq!(err, "no configuration file to reload");
        generation.stop().await.expect("stops");
    }
}
