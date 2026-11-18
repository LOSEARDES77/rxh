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

use rxh_core::config::Config;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::admin::{self, AdminState, Control, Current};
use crate::error::Error;
use crate::master::{Master, ServerInfo};
use crate::registry::Registry;

/// Reads and validates a configuration file.
pub fn load_config(path: &Path) -> Result<Config, Error> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?;
    Ok(Config::from_toml(&text)?)
}

/// Daemon bound to one configuration file.
#[derive(Debug, Clone)]
pub struct Daemon {
    config_path: PathBuf,
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
            config_path: config_path.into(),
        }
    }

    /// Runs until `SIGTERM`, `SIGINT` or `POST /stop`. `SIGHUP` and
    /// `POST /reload` reload the configuration file.
    pub async fn run(self) -> Result<(), Error> {
        let config = load_config(&self.config_path)?;
        let admin_settings = config.admin.clone();
        let mut generation = Generation::start(config, None)?;

        let (control, mut controls) = mpsc::channel(8);
        let admin_state = Arc::new(AdminState {
            started: Instant::now(),
            config_path: std::fs::canonicalize(&self.config_path).ok(),
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

    async fn reload(&self, generation: &mut Generation, admin: &AdminState) -> Result<(), String> {
        println!("Main => Reloading {}", self.config_path.display());
        let config = load_config(&self.config_path).map_err(|err| err.to_string())?;

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
