//! Long running process: loads the configuration, runs it, serves the admin
//! API and reloads without dropping connections.
//!
//! A reload builds a complete new generation (pools, listeners, health
//! checks) next to the running one. Listeners use `SO_REUSEPORT`, so the new
//! generation binds the same ports, then the old one stops accepting and
//! drains its connections in the background. If the configuration is invalid
//! the old generation keeps running untouched.

use std::collections::BTreeMap;
use std::env::VarError;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use rxh_core::config::{
    AdminListen, Catalog, CatalogKind, Config, ConfigError, Docker, IncludedConfig, Runtime, Tls,
};
use rxh_core::placeholder::{self, Lookup};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::access::Store;
use crate::acme;
use crate::admin::{self, AdminState, Control, Current};
use crate::catalog;
use crate::docker::{self, Container, Provider, Route};
use crate::error::Error;
use crate::http3::{self, Endpoints};
use crate::include;
use crate::master::{Master, ServerInfo};
use crate::per_core;
use crate::registry::Registry;
use crate::session::Sessions;
use crate::systemd;
use crate::tls::AdminTls;
use crate::upgrade::{self, Lease};

fn read_file(path: &Path) -> Result<String, Error> {
    Ok(std::fs::read_to_string(path)
        .map_err(|err| io::Error::new(err.kind(), format!("{}: {err}", path.display())))?)
}

/// Reads a configuration file and, when `expand` is true, replaces its
/// `{env.NAME}` and `{file.PATH}` placeholders. Relative secret file paths
/// start at the directory of `path`.
fn read_config(path: &Path, expand: bool) -> Result<String, Error> {
    let text = read_file(path)?;
    if !expand {
        return Ok(text);
    }
    let base = path.parent().unwrap_or(Path::new(""));
    let expanded = placeholder::expand(&text, |lookup| match lookup {
        Lookup::Env(name) => std::env::var(name).map_err(|err| match err {
            VarError::NotPresent => format!("environment variable {name} is not set"),
            VarError::NotUnicode(_) => format!("environment variable {name} is not UTF-8"),
        }),
        Lookup::File(file) => {
            let file = base.join(file);
            // `echo secret > file` and most secret stores end the value with a newline.
            std::fs::read_to_string(&file)
                .map(|contents| contents.trim_end_matches(['\r', '\n']).to_owned())
                .map_err(|err| format!("{}: {err}", file.display()))
        }
    })
    .map_err(|err| ConfigError::Invalid(format!("{}: {err}", path.display())))?;
    Ok(expanded.into_owned())
}

/// Reads the main file and every file it includes, merged but not validated.
fn read_merged(path: &Path, expand: bool) -> Result<(Config, Vec<PathBuf>), Error> {
    let mut config = Config::parse(&read_config(path, expand)?)?;
    let included = include::expand(path, &config.include)?;
    for file in &included {
        let part = IncludedConfig::from_toml(&read_config(file, expand)?)
            .map_err(|err| ConfigError::Invalid(format!("{}: {err}", file.display())))?;
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
    let (config, _) = read_merged(path, /* expand= */ true)?;
    config.validate()?;
    Ok(config)
}

/// The main configuration file followed by the files it includes, sorted by
/// path. The files are parsed but the result is not validated, and
/// placeholders are not replaced, so the CLI can edit the files without the
/// secrets of the daemon.
pub fn config_files(path: &Path) -> Result<Vec<PathBuf>, Error> {
    Ok(read_merged(path, /* expand= */ false)?.1)
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
    /// Docker containers and catalog instances the pools were last
    /// synchronized with, sorted by source.
    containers: Vec<Container>,
    /// Docker routes the listeners were built for.
    routes: Vec<Route>,
    /// Containers, instances and pools that are not routed, already logged.
    ignored: Vec<String>,
}

impl Generation {
    /// Starts `config` with routes to the Docker `containers`, and hands the
    /// new HTTP/3 connections of `endpoints` to it. Relative `[acme]` paths
    /// are joined to `base`.
    async fn start(
        config: Config,
        previous: Option<&Registry>,
        base: &Path,
        containers: Vec<Container>,
        endpoints: &Endpoints,
    ) -> Result<Self, Error> {
        // What runs has the implicit HTTP to HTTPS redirect servers and the
        // Docker routes; what the admin API shows and edits is the
        // configuration as written.
        let (effective, plan) = docker::plan(&config.with_https_redirects(), &containers);
        let registry = Arc::new(Registry::build(&effective)?);
        // Docker members first, so they keep their drain state too.
        plan.sync(&registry);
        if let Some(previous) = previous {
            registry.inherit(previous);
        }
        // A `udp` server taking over the port of a QUIC endpoint must be
        // able to bind it.
        endpoints.release(&effective).await;
        // The last step that can fail, before the listeners serve: a
        // generation that does not start leaves the QUIC endpoints to the
        // running one.
        let install = |servers: &[ServerInfo]| endpoints.install(&http3_listeners(servers));
        let (stop, stopped) = oneshot::channel::<()>();
        let (servers, acme, task) = match effective.runtime {
            Runtime::Shared => {
                let master =
                    Master::init(&effective, Arc::clone(&registry))?.shutdown_on(async move {
                        let _ = stopped.await;
                    });
                let acme = acme::Manager::new(&effective, &master, base);
                let servers = master.servers();
                install(&servers)?;
                (servers, acme, tokio::task::spawn(master.run()))
            }
            Runtime::PerCore => {
                // Its listeners already serve; on error, dropping `stop`
                // stops them.
                let started = per_core::start(effective, &registry, base, stopped).await?;
                install(&started.servers)?;
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
            containers,
            routes: plan.routes,
            ignored: plan.ignored,
        })
    }

    /// Hands the QUIC endpoints back to this running generation after a
    /// generation that failed to start released or replaced some of them.
    fn restore_http3(&self, endpoints: &Endpoints) {
        if let Err(err) = endpoints.install(&http3_listeners(&self.servers)) {
            eprintln!("Main => HTTP/3 of the running configuration not restored: {err}");
        }
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
    /// Hot binary upgrade, from `SIGUSR2` or from the admin API, which gets
    /// the outcome.
    Upgrade(Option<oneshot::Sender<Result<(), String>>>),
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
    /// `POST /reload` reload the configuration file; `SIGUSR2` and
    /// `POST /upgrade` hand the listening sockets to a new process.
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
        // Before anything listens: a certificate that cannot be read stops
        // the start.
        let admin_tls = match &admin_settings.tls {
            Some(Tls::Files { cert, key }) if admin_settings.enabled => {
                let tls = AdminTls::from_files(cert, key)
                    .map_err(|err| io::Error::new(err.kind(), format!("[admin] tls: {err}")))?;
                Some(tls)
            }
            // Validation refuses `"auto"` in `[admin]`.
            Some(_) | None => None,
        };
        // Before the listeners too: the first random bytes of a process take
        // tens of milliseconds, in which the servers would answer and the
        // admin API would not.
        let sessions = Sessions::new();
        let access = Store::open(PathBuf::from(&admin_settings.users));
        // QUIC sockets outlive generations, see `http3`.
        let endpoints = Endpoints::default();
        let mut generation =
            Generation::start(config, None, self.base_dir(), Vec::new(), &endpoints).await?;

        let (control, mut controls) = mpsc::channel(8);
        let admin_state = Arc::new(AdminState {
            started: Instant::now(),
            config_path: self
                .config_path()
                .and_then(|path| std::fs::canonicalize(path).ok()),
            listen: admin_settings.listen.clone(),
            token: admin_settings.token.clone(),
            tls: admin_tls,
            current: RwLock::new(generation.current()),
            control,
            saving: tokio::sync::Mutex::new(()),
            hashing: admin::hashing_permits(),
            access,
            sessions,
        });

        let admin_listener: Option<(AbortOnDrop, Lease)> = if admin_settings.enabled {
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
                AdminListen::Tcp(address) if admin_state.tls.is_some() => {
                    println!("Admin => API listening on https://{address}");
                }
                AdminListen::Tcp(address) => println!("Admin => API listening on http://{address}"),
                AdminListen::Unix(path) => {
                    println!("Admin => API listening on unix socket {}", path.display());
                }
            }
            if admin::announces_setup_code(
                admin_state.tls.is_some(),
                admin_state.token.is_some(),
                !admin_state.access.read().user.is_empty(),
            ) {
                println!(
                    "Admin => no user exists yet; to create the first one from another machine \
                     use the setup code {}",
                    admin_state.sessions.setup_code()
                );
            }
            let lease = listener.lease()?;
            let task = admin::serve(listener, Arc::clone(&admin_state));
            Some((AbortOnDrop(tokio::task::spawn(task)), lease))
        } else {
            None
        };

        systemd::warn_unused();
        systemd::notify("READY=1\nSTATUS=Running");
        upgrade::ready();
        // Outcome of the hot binary upgrade in progress.
        let mut upgrading: Option<JoinHandle<Result<(), String>>> = None;
        let mut successor: u32 = 0;
        let mut upgrade_reply: Option<oneshot::Sender<Result<(), String>>> = None;

        let (docker_updates, mut containers) = mpsc::channel::<(Provider, Vec<Container>)>(1);
        // Settings being followed and the task that follows them.
        let mut docker_watch: Option<(Docker, AbortOnDrop)> = None;
        let mut catalog_watches: BTreeMap<CatalogKind, (Catalog, AbortOnDrop)> = BTreeMap::new();

        let mut signals = Signals::new()?;
        let mut polls = tokio::time::interval(WATCH_INTERVAL);
        polls.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            // After every change of configuration: `[docker]` may be new,
            // gone or point somewhere else.
            let settings = generation.config.docker.as_ref();
            if settings != docker_watch.as_ref().map(|(settings, _)| settings) {
                docker_watch = settings.map(|settings| {
                    println!(
                        "Docker => Following the {:?} containers of {}",
                        settings.provider,
                        settings.endpoint()
                    );
                    let task = docker::watch(settings.clone(), docker_updates.clone());
                    (settings.clone(), AbortOnDrop(tokio::task::spawn(task)))
                });
            }
            for kind in CatalogKind::ALL {
                let settings = generation.config.catalog(kind);
                if settings == catalog_watches.get(&kind).map(|(settings, _)| settings) {
                    continue;
                }
                match settings {
                    Some(settings) => {
                        println!(
                            "{} => Following the services of {}",
                            Provider::Catalog(kind).label(),
                            settings.endpoint(kind)
                        );
                        let task = catalog::watch(kind, settings.clone(), docker_updates.clone());
                        let task = AbortOnDrop(tokio::task::spawn(task));
                        catalog_watches.insert(kind, (settings.clone(), task));
                    }
                    None => {
                        catalog_watches.remove(&kind);
                    }
                }
            }

            let event = tokio::select! {
                event = signals.next() => event,
                Some((source, update)) = containers.recv() => {
                    let mut merged = generation.containers.clone();
                    merged.retain(|container| container.source != source);
                    merged.extend(update);
                    merged.sort_by_key(|container| container.source);
                    self.follow_docker(source, merged, &mut generation, &admin_state, &endpoints)
                        .await;
                    continue;
                }
                Some(control) = controls.recv() => match control {
                    Control::Stop => Event::Stop,
                    Control::Upgrade(reply) => Event::Upgrade(Some(reply)),
                    Control::Reload(reply) => {
                        if let Some(watcher) = &mut watcher {
                            watcher.loading();
                        }
                        let outcome = self.reload(&mut generation, &admin_state, &endpoints).await;
                        let _ = reply.send(outcome);
                        continue;
                    }
                    Control::Apply(change, reply) => {
                        println!("Main => Applying a configuration change from the admin API");
                        // Patched on top of the live members, so backends
                        // added at runtime survive the swap.
                        let outcome = match change.apply(&generation.current().live_config()) {
                            Ok(config) => {
                                let containers = generation.containers.clone();
                                self.apply(
                                    config,
                                    containers,
                                    &mut generation,
                                    &admin_state,
                                    &endpoints,
                                )
                                .await
                            }
                            Err(message) => Err(message),
                        };
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
                outcome = async { upgrading.as_mut().expect("guarded by the condition").await },
                    if upgrading.is_some() =>
                {
                    upgrading = None;
                    let outcome = outcome.map_err(|err| err.to_string()).flatten();
                    if let Some(reply) = upgrade_reply.take() {
                        let _ = reply.send(outcome.clone());
                    }
                    match outcome {
                        Ok(()) => {
                            println!("Main => Process {successor} took over the listening sockets");
                            // systemd follows the new process from here on,
                            // so this one must not report that it stops.
                            systemd::notify(&format!("MAINPID={successor}"));
                            // The admin socket is shared too: requests go
                            // to the new process alone while this one
                            // drains. Requests in flight still get answers.
                            drop(admin_listener);
                            // The UDP sockets are not handed over: requests
                            // in flight over HTTP/3 fail and clients retry.
                            endpoints.close();
                            return generation.stop().await;
                        }
                        Err(message) => {
                            eprintln!("Main => Upgrade failed, this process keeps serving: {message}");
                        }
                    }
                    continue;
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
                Event::Upgrade(reply) => {
                    let started = if upgrading.is_some() {
                        Err(String::from("an upgrade is already in progress"))
                    } else {
                        upgrade::spawn()
                            .map_err(|err| format!("cannot start the new process: {err}"))
                    };
                    match started {
                        Ok((pid, wait)) => {
                            println!("Main => Upgrading: starting process {pid}");
                            successor = pid;
                            upgrading = Some(tokio::task::spawn_blocking(wait));
                            upgrade_reply = reply;
                        }
                        Err(message) => {
                            eprintln!("Main => {message}");
                            if let Some(reply) = reply {
                                let _ = reply.send(Err(message));
                            }
                        }
                    }
                }
                Event::Reload | Event::Watch => {
                    if let (Event::Reload, Some(watcher)) = (&event, &mut watcher) {
                        watcher.loading();
                    }
                    if let Err(message) =
                        self.reload(&mut generation, &admin_state, &endpoints).await
                    {
                        eprintln!(
                            "Main => Reload failed, keeping the running configuration: {message}"
                        );
                    }
                }
            }
        }

        println!("Main => Shutting down");
        systemd::notify("STOPPING=1");
        let stopped = generation.stop().await;
        endpoints.close();
        stopped
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

    /// Reloads the configuration file, reporting the reload to systemd.
    async fn reload(
        &self,
        generation: &mut Generation,
        admin: &AdminState,
        endpoints: &Endpoints,
    ) -> Result<(), String> {
        systemd::reloading();
        let outcome = self.reload_file(generation, admin, endpoints).await;
        // The previous configuration keeps running after a failure, so the
        // service is ready either way.
        systemd::notify(&match &outcome {
            Ok(()) => "READY=1\nSTATUS=Running".to_owned(),
            Err(message) => format!(
                "READY=1\nSTATUS=Reload failed: {}",
                message.replace('\n', " ")
            ),
        });
        outcome
    }

    async fn reload_file(
        &self,
        generation: &mut Generation,
        admin: &AdminState,
        endpoints: &Endpoints,
    ) -> Result<(), String> {
        let path = self
            .config_path()
            .ok_or("no configuration file to reload")?;
        println!("Main => Reloading {}", path.display());
        let config = load_config(path).map_err(|err| err.to_string())?;
        let containers = generation.containers.clone();
        self.apply(config, containers, generation, admin, endpoints)
            .await
    }

    /// Takes in the containers Docker runs and the catalog instances now.
    /// Replicas that come and go
    /// only change the members of the running pools; a change of routes
    /// swaps the generation, as a reload does.
    async fn follow_docker(
        &self,
        source: Provider,
        containers: Vec<Container>,
        generation: &mut Generation,
        admin: &AdminState,
        endpoints: &Endpoints,
    ) {
        if containers == generation.containers {
            return;
        }
        let (_, plan) = docker::plan(&generation.config.with_https_redirects(), &containers);
        docker::log_ignored(&plan.ignored, &generation.ignored);
        generation.ignored.clone_from(&plan.ignored);
        if plan.routes == generation.routes {
            plan.sync(&generation.registry);
            generation.containers = containers;
            return;
        }
        println!(
            "Main => {} {}s changed, updating the routes",
            source.label(),
            source.noun()
        );
        // On top of the live members, like a change from the admin API.
        let config = generation.current().live_config();
        if let Err(message) = self
            .apply(config, containers, generation, admin, endpoints)
            .await
        {
            eprintln!("Main => Provider routes not updated, keeping the running ones: {message}");
        }
    }

    /// Swaps the running generation for one of `config`, which is valid,
    /// routing to the Docker `containers`.
    async fn apply(
        &self,
        config: Config,
        containers: Vec<Container>,
        generation: &mut Generation,
        admin: &AdminState,
        endpoints: &Endpoints,
    ) -> Result<(), String> {
        // A server taking over the port of the challenge-only listener must
        // be able to bind it.
        acme::release_challenge_port(&config.with_https_redirects()).await;

        if config.admin.listen != admin.listen || config.admin.token != admin.token {
            eprintln!("Main => [admin] changes take effect after a restart");
        }

        let previous_registry = Arc::clone(&generation.registry);
        let started = Generation::start(
            config.clone(),
            Some(&previous_registry),
            self.base_dir(),
            containers.clone(),
            endpoints,
        )
        .await;
        let next = match started {
            Ok(next) => next,
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::AddrInUse => {
                // UDP sockets and platforms without SO_REUSEPORT cannot bind
                // twice: stop first, then start, restoring the old
                // configuration if the new one still fails.
                let previous = Config::clone(&generation.config);
                let previous_containers = generation.containers.clone();
                let old = std::mem::replace(generation, placeholder(endpoints).await?);
                old.stop().await.map_err(|err| err.to_string())?;
                let base = self.base_dir();
                let started = Generation::start(
                    config,
                    Some(&previous_registry),
                    base,
                    containers,
                    endpoints,
                )
                .await;
                match started {
                    Ok(next) => next,
                    Err(err) => {
                        *generation = Generation::start(
                            previous,
                            Some(&previous_registry),
                            base,
                            previous_containers,
                            endpoints,
                        )
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
            Err(err) => {
                generation.restore_http3(endpoints);
                return Err(err.to_string());
            }
        };

        // A new configuration can clash with other containers.
        docker::log_ignored(&next.ignored, &generation.ignored);
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

/// What `servers` serve over HTTP/3.
fn http3_listeners(servers: &[ServerInfo]) -> Vec<Arc<http3::Listener>> {
    servers
        .iter()
        .filter_map(|server| server.http3.clone())
        .collect()
}

/// Empty generation used while swapping.
async fn placeholder(endpoints: &Endpoints) -> Result<Generation, String> {
    Generation::start(
        Config::default(),
        None,
        Path::new(""),
        Vec::new(),
        endpoints,
    )
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
    upgrade: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Signals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            upgrade: signal(SignalKind::user_defined2())?,
        })
    }

    async fn next(&mut self) -> Event {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Event::Stop,
            _ = self.terminate.recv() => Event::Stop,
            _ = self.hangup.recv() => Event::Reload,
            _ = self.upgrade.recv() => Event::Upgrade(None),
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

    #[test]
    fn secret_files_are_read_relative_to_the_file_that_names_them() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("rxh.toml");
        write(dir.path(), "rxh.toml", "include = [\"conf.d/*.toml\"]\n");
        write(
            dir.path(),
            "conf.d/api.toml",
            "[upstream.api]\nbackends = [\"{file.secrets/backend}\"]\n",
        );
        write(dir.path(), "conf.d/secrets/backend", "127.0.0.1:9\r\n");
        let config = load_config(&main).expect("loads");
        assert_eq!(config.upstreams["api"].backends[0].address, "127.0.0.1:9");
        // Listing the files for the CLI does not need the secrets.
        fs::remove_file(dir.path().join("conf.d/secrets/backend")).expect("remove");
        assert_eq!(config_files(&main).expect("files").len(), 2);
        let err = load_config(&main).expect_err("missing secret").to_string();
        assert!(
            err.contains("api.toml") && err.contains("{file.secrets/backend}"),
            "{err}"
        );

        write(
            dir.path(),
            "rxh.toml",
            "[admin]\ntoken = \"{env.RXH_TEST_SURELY_UNSET_VARIABLE}\"\n",
        );
        let err = load_config(&main).expect_err("unset variable").to_string();
        assert!(
            err.contains("environment variable RXH_TEST_SURELY_UNSET_VARIABLE is not set"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn daemon_without_a_file_cannot_reload() {
        let config = Config::from_toml("[admin]\nenabled = false\n").expect("valid");
        let daemon = Daemon::from_config(config);
        assert!(daemon.config_path().is_none());
        let endpoints = Endpoints::default();
        let mut generation = placeholder(&endpoints).await.expect("generation");
        let admin = {
            let (control, _) = mpsc::channel(1);
            AdminState {
                started: Instant::now(),
                config_path: None,
                listen: "127.0.0.1:0".parse().expect("address"),
                token: None,
                tls: None,
                current: RwLock::new(generation.current()),
                control,
                saving: tokio::sync::Mutex::new(()),
                hashing: admin::hashing_permits(),
                access: Store::open(PathBuf::new()),
                sessions: Sessions::new(),
            }
        };
        let err = daemon
            .reload(&mut generation, &admin, &endpoints)
            .await
            .expect_err("no file");
        assert_eq!(err, "no configuration file to reload");
        generation.stop().await.expect("stops");
    }
}
