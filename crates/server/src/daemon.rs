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
use std::time::{Duration, Instant};

use rxh_core::config::{AdminListen, Config, IncludedConfig, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::acme;
use crate::admin::{self, AdminState, Control, Current};
use crate::error::Error;
use crate::include;
use crate::master::{Master, ServerInfo};
use crate::per_core;
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
    Memory(Box<Config>),
}

/// One running configuration.
struct Generation {
    config: Arc<Config>,
    registry: Arc<Registry>,
    servers: Vec<ServerInfo>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), Error>>,
    /// Health checks, DNS refreshes and the ACME manager.
    health_checks: JoinSet<()>,
}

impl Generation {
    /// Starts `config`. Relative `[acme]` paths are joined to `base`.
    async fn start(
        config: Config,
        previous: Option<&Registry>,
        base: &Path,
    ) -> Result<Self, Error> {
        // What runs has the implicit HTTP to HTTPS redirect servers; what the
        // admin API shows and edits is the configuration as written.
        let effective = config.with_https_redirects();
        let registry = Arc::new(Registry::build(&effective)?);
        if let Some(previous) = previous {
            registry.inherit_dynamic(previous);
        }
        let (stop, stopped) = oneshot::channel::<()>();
        let (servers, acme, task) = match effective.runtime {
            Runtime::Shared => {
                let master =
                    Master::init(&effective, Arc::clone(&registry))?.shutdown_on(async move {
                        let _ = stopped.await;
                    });
                let acme = acme::Manager::new(&effective, &master, base);
                (master.servers(), acme, tokio::task::spawn(master.run()))
            }
            Runtime::PerCore => {
                let started = per_core::start(effective, &registry, base, stopped).await?;
                (started.servers, started.acme, started.task)
            }
        };
        // Health checks, the admin API and ACME stay on the calling runtime.
        let mut health_checks = registry.start_background_tasks();
        if let Some(acme) = acme {
            health_checks.spawn(acme.run());
        }

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
    /// `watch = true` noticed an edit; the watcher already took it in.
    Watch,
    Stop,
}

/// How often `watch = true` reads the configuration files.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// Contents of the configuration file and of the files it includes. `None`
/// for a file that cannot be read.
#[derive(Clone, PartialEq)]
struct Snapshot(Vec<(PathBuf, Option<Vec<u8>>)>);

impl Snapshot {
    fn take(path: &Path) -> Self {
        // A file that does not parse hides its includes; the list grows back
        // once it is fixed, which is a change too.
        let files: Vec<PathBuf> = config_files(path).unwrap_or_else(|_| vec![path.to_owned()]);
        Self(
            files
                .into_iter()
                .map(|file| {
                    let contents: Option<Vec<u8>> = std::fs::read(&file).ok();
                    (file, contents)
                })
                .collect(),
        )
    }
}

/// Polls the configuration files for `watch = true`. Contents are compared
/// rather than modification times, which can be coarse or restored by tools.
struct Watcher {
    path: PathBuf,
    /// Contents of the last reload attempt, successful or not: a rejected
    /// edit is not retried until the files change again.
    loaded: Snapshot,
    /// Contents at the previous poll. A change is reloaded once it stayed the
    /// same for a whole interval, so a file still being written is not read.
    last: Snapshot,
}

impl Watcher {
    fn new(path: &Path) -> Self {
        let loaded = Snapshot::take(path);
        Self {
            path: path.to_owned(),
            last: loaded.clone(),
            loaded,
        }
    }

    /// Records the files a reload requested elsewhere is about to read.
    fn loading(&mut self) {
        self.loaded = Snapshot::take(&self.path);
        self.last = self.loaded.clone();
    }

    /// Whether the files changed and settled since the last reload.
    fn poll(&mut self) -> bool {
        let now = Snapshot::take(&self.path);
        let settled = now == self.last && now != self.loaded;
        if settled {
            self.loaded = now.clone();
        }
        self.last = now;
        settled
    }
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
            source: Source::Memory(Box::new(config)),
        }
    }

    /// Runs until `SIGTERM`, `SIGINT` or `POST /stop`. `SIGHUP` and
    /// `POST /reload` reload the configuration file.
    pub async fn run(self) -> Result<(), Error> {
        // Taken before loading, so an edit made while starting is noticed.
        let mut watcher = self.config_path().map(Watcher::new);
        let config = match &self.source {
            Source::Memory(config) => {
                config.validate()?;
                Config::clone(config)
            }
            Source::File(path) => load_config(path)?,
        };
        let admin_settings = config.admin.clone();
        let mut generation = Generation::start(config, None, self.base_dir()).await?;

        let (control, mut controls) = mpsc::channel(8);
        let admin_state = Arc::new(AdminState {
            started: Instant::now(),
            config_path: self
                .config_path()
                .and_then(|path| std::fs::canonicalize(path).ok()),
            listen: admin_settings.listen.clone(),
            token: admin_settings.token.clone(),
            current: RwLock::new(generation.current()),
            control,
        });

        let _admin = if admin_settings.enabled {
            let listener = admin::Listener::bind(&admin_settings.listen).await.map_err(|err| {
                io::Error::new(
                    err.kind(),
                    format!(
                        "admin API on {}: {err} (is another rxh running? set [admin] listen or enabled = false)",
                        admin_settings.listen
                    ),
                )
            })?;
            match &admin_settings.listen {
                AdminListen::Tcp(address) => println!("Admin => API listening on http://{address}"),
                AdminListen::Unix(path) => {
                    println!("Admin => API listening on unix socket {}", path.display());
                }
            }
            Some(AbortOnDrop(tokio::task::spawn(admin::serve(
                listener,
                Arc::clone(&admin_state),
            ))))
        } else {
            None
        };

        let mut signals = Signals::new()?;
        let mut polls = tokio::time::interval(WATCH_INTERVAL);
        polls.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let event = tokio::select! {
                event = signals.next() => event,
                Some(control) = controls.recv() => match control {
                    Control::Stop => Event::Stop,
                    Control::Reload(reply) => {
                        if let Some(watcher) = &mut watcher {
                            watcher.loading();
                        }
                        let outcome = self.reload(&mut generation, &admin_state).await;
                        let _ = reply.send(outcome);
                        continue;
                    }
                },
                _ = polls.tick(), if generation.config.watch => {
                    if !watcher.as_mut().is_some_and(Watcher::poll) {
                        continue;
                    }
                    println!("Main => Configuration file changed");
                    Event::Watch
                }
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
                Event::Reload | Event::Watch => {
                    if let (Event::Reload, Some(watcher)) = (&event, &mut watcher) {
                        watcher.loading();
                    }
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

    /// Directory relative `[acme]` paths are resolved against: the one of
    /// the configuration file, else the current directory.
    fn base_dir(&self) -> &Path {
        self.config_path()
            .and_then(Path::parent)
            .unwrap_or(Path::new(""))
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
        // A server taking over the port of the challenge-only listener must
        // be able to bind it.
        acme::release_challenge_port(&config.with_https_redirects()).await;

        if config.admin.listen != admin.listen || config.admin.token != admin.token {
            eprintln!("Main => [admin] changes take effect after a restart");
        }

        let previous_registry = Arc::clone(&generation.registry);
        let started =
            Generation::start(config.clone(), Some(&previous_registry), self.base_dir()).await;
        let next = match started {
            Ok(next) => next,
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::AddrInUse => {
                // UDP sockets and platforms without SO_REUSEPORT cannot bind
                // twice: stop first, then start, restoring the old
                // configuration if the new one still fails.
                let previous = Config::clone(&generation.config);
                let old = std::mem::replace(generation, placeholder().await?);
                old.stop().await.map_err(|err| err.to_string())?;
                match Generation::start(config, Some(&previous_registry), self.base_dir()).await {
                    Ok(next) => next,
                    Err(err) => {
                        *generation =
                            Generation::start(previous, Some(&previous_registry), self.base_dir())
                                .await
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
async fn placeholder() -> Result<Generation, String> {
    Generation::start(Config::default(), None, Path::new(""))
        .await
        .map_err(|err| err.to_string())
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
    fn watcher_reloads_settled_changes_of_every_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("rxh.toml");
        write(dir.path(), "rxh.toml", "include = [\"conf.d/*.toml\"]\n");
        let mut watcher = Watcher::new(&main);
        assert!(!watcher.poll(), "nothing changed");

        // Reloaded on the poll after the one that saw the change.
        write(dir.path(), "conf.d/a.toml", &server("a"));
        assert!(!watcher.poll(), "not settled yet");
        assert!(watcher.poll(), "new included file");
        assert!(!watcher.poll(), "reloaded once");

        write(dir.path(), "conf.d/a.toml", &server("b"));
        assert!(!watcher.poll());
        write(dir.path(), "conf.d/a.toml", &server("c"));
        assert!(!watcher.poll(), "still being written");
        assert!(watcher.poll(), "edited included file");

        // A broken main file is tried once, then fixing it is a change.
        write(dir.path(), "rxh.toml", "include = [");
        assert!(!watcher.poll());
        assert!(watcher.poll(), "broken file is tried");
        assert!(!watcher.poll(), "and not retried");
        write(dir.path(), "rxh.toml", "include = [\"conf.d/*.toml\"]\n");
        assert!(!watcher.poll());
        assert!(watcher.poll(), "fixed file");

        // A reload requested elsewhere already read the edit.
        fs::remove_file(dir.path().join("conf.d/a.toml")).expect("remove");
        watcher.loading();
        assert!(!watcher.poll());
        assert!(!watcher.poll());
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
        let mut generation = placeholder().await.expect("generation");
        let admin = {
            let (control, _) = mpsc::channel(1);
            AdminState {
                started: Instant::now(),
                config_path: None,
                listen: "127.0.0.1:0".parse().expect("address"),
                token: None,
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
