//! `runtime = "per_core"`: one single-threaded runtime per CPU instead of
//! one work-stealing runtime.
//!
//! Every runtime has its own thread, its own `SO_REUSEPORT` listener on each
//! address and its own upstream connections, so a request stays on one CPU
//! from accept to response. The kernel spreads new connections over the
//! listeners. Routes, pools, limits and counters are shared:
//!
//! ```text
//!        +----------+   +----------+   +----------+
//!        | Master 0 |   | Master 1 |   | Master N |   one thread each
//!        +----------+   +----------+   +----------+
//!          |      |       |      |       |      |
//!        :80    :443    :80    :443    :80    :443    SO_REUSEPORT
//! ```
//!
//! The first master builds the listeners; the others bind replicas of them
//! (see [`Master::from_replicas`]). UDP sockets exist only in the first.

use std::cell::Cell;
use std::io;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use rxh_core::config::{Config, Runtime};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::acme;
use crate::error::Error;
use crate::master::{Master, ServerInfo, TcpListeners};
use crate::registry::Registry;
use crate::server::Replica;

thread_local! {
    static INDEX: Cell<usize> = const { Cell::new(0) };
}

/// Position of the per core runtime that owns the calling thread, 0 on any
/// other thread.
pub(crate) fn index() -> usize {
    INDEX.get()
}

/// Number of runtimes that serve connections with `runtime`: one per CPU
/// this process may run on, or the single shared one.
pub(crate) fn count(runtime: Runtime) -> usize {
    match runtime {
        Runtime::Shared => 1,
        Runtime::PerCore => thread::available_parallelism().map_or(1, NonZero::get),
    }
}

/// A configuration running on per core runtimes.
pub(crate) struct Started {
    pub(crate) tcp_listeners: Vec<TcpListeners>,

    /// Listeners of the first runtime, which stand for their replicas in
    /// status reports.
    pub(crate) servers: Vec<ServerInfo>,

    /// Certificate manager of the listeners, if they need one.
    pub(crate) acme: Option<acme::Manager>,

    /// Ends when every runtime has shut down.
    pub(crate) task: JoinHandle<Result<(), Error>>,
}

/// What the first runtime reports once its listeners are bound.
struct First {
    servers: Vec<ServerInfo>,
    acme: Option<acme::Manager>,
    replicas: Vec<Replica>,
}

struct Ready {
    first: Option<First>,
    listeners: Vec<TcpListeners>,
}

struct Worker {
    index: usize,
    config: Arc<Config>,
    registry: Arc<Registry>,
    base: PathBuf,
    /// Listeners to replicate, `None` in the first runtime.
    replicas: Option<Vec<Replica>>,
    stop: watch::Receiver<bool>,
    previous: Vec<TcpListeners>,
    ready: oneshot::Sender<Result<Ready, Error>>,
    done: mpsc::UnboundedSender<Result<(), Error>>,
}

impl Worker {
    /// Body of the thread: binds the listeners, reports through `ready`,
    /// serves until `stop` and reports the outcome through `done`.
    fn run(self) {
        INDEX.set(self.index);
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = self.ready.send(Err(err.into()));
                return;
            }
        };

        runtime.block_on(async move {
            let master = match &self.replicas {
                None => Master::init_with_tcp(&self.config, self.registry, &self.previous),
                Some(replicas) => {
                    Master::from_replicas_with_tcp(replicas, self.registry, self.previous.first())
                }
            };
            let master = match master {
                Ok(master) => master,
                Err(err) => {
                    let _ = self.ready.send(Err(err));
                    return;
                }
            };
            let first = self.replicas.is_none().then(|| First {
                servers: master.servers(),
                acme: acme::Manager::new(&self.config, &master, &self.base),
                replicas: master.replicas(),
            });
            let _ = self.ready.send(Ok(Ready {
                first,
                listeners: master.tcp_listeners(),
            }));

            let mut stop = self.stop;
            let master = master.shutdown_on(async move {
                // An error means the daemon dropped the generation: stop too.
                let _ = stop.wait_for(|stop| *stop).await;
            });
            let _ = self.done.send(master.run().await);
        });
    }
}

/// Starts the listeners of `config` on [`count`] threads, each with its own
/// runtime, and stops them when `stopped` completes or one of them fails.
/// Relative `[acme]` paths are joined to `base`.
pub(crate) async fn start(
    config: Config,
    registry: &Arc<Registry>,
    base: &Path,
    stopped: oneshot::Receiver<()>,
    previous: &[TcpListeners],
) -> Result<Started, Error> {
    let count = count(config.runtime);
    let config = Arc::new(config);
    // Dropped on an early return, which stops the runtimes already started.
    let stop = watch::Sender::new(false);
    let (done, mut finished) = mpsc::unbounded_channel();

    let mut first: Option<First> = None;
    let mut tcp_listeners = Vec::with_capacity(count);
    let mut extra_listeners = Vec::new();
    for index in 0..count {
        let (ready, started) = oneshot::channel();
        let mut reuse: Vec<_> = previous.get(index).cloned().into_iter().collect();
        if index == 0 && previous.len() > count {
            reuse.extend_from_slice(&previous[count..]);
        }
        let worker = Worker {
            index,
            config: Arc::clone(&config),
            registry: Arc::clone(registry),
            base: base.to_owned(),
            replicas: first.as_ref().map(|first| first.replicas.clone()),
            stop: stop.subscribe(),
            previous: reuse,
            ready,
            done: done.clone(),
        };
        thread::Builder::new()
            .name(format!("rxh-core-{index}"))
            .spawn(move || worker.run())?;
        match started.await {
            Ok(Ok(Ready {
                first: report,
                listeners,
            })) => {
                if report.is_some() {
                    first = report;
                }
                let mut listeners = listeners.into_iter();
                tcp_listeners.push(listeners.next().unwrap_or_default());
                extra_listeners.extend(listeners);
            }
            Ok(Err(err)) => return Err(err),
            Err(_) => return Err(io::Error::other("a per core runtime died on start").into()),
        }
    }
    drop(done);
    let first = first.expect("there is at least one runtime, and the first one reports");
    println!("Main => {count} per core runtimes");

    let task = tokio::task::spawn(async move {
        let mut first_error = None;
        tokio::select! {
            _ = stopped => {}
            Some(result) = finished.recv() => {
                eprintln!("Main => A per core runtime stopped, shutting down the rest");
                first_error = result.err();
            }
        }
        stop.send_replace(true);
        while let Some(result) = finished.recv().await {
            if let Err(err) = result {
                first_error.get_or_insert(err);
            }
        }
        first_error.map_or(Ok(()), Err)
    });

    tcp_listeners.extend(extra_listeners);
    Ok(Started {
        tcp_listeners,
        servers: first.servers,
        acme: first.acme,
        task,
    })
}
