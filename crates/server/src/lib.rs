//! Runtime of RXH: listeners, the HTTP reverse proxy, layer 4 (TCP and UDP)
//! load balancing, active health checks, the admin API and the daemon loop.
//!
//! The configuration model and the load balancing engine live in
//! [`rxh_core`]. This crate puts them on the network with [`tokio`] and
//! [`hyper`].

mod acme;
mod admin;
mod daemon;
mod dynamic;
mod error;
mod files;
mod health;
mod hooks;
mod http;
mod include;
mod l4match;
mod log;
mod master;
mod metrics;
mod proxy_protocol;
mod registry;
mod server;
mod stream;
mod tcp;
mod timeout;
mod tls;
mod udp;
mod upstream;

pub use daemon::{Daemon, config_files, load_config};
pub use error::Error;
pub use include::covers as include_covers;
pub use master::{Master, ServerInfo};
pub use registry::{Group, Registry};
pub use rxh_core::{VERSION, config, pool};
pub use server::{Server, ShutdownState, State};
pub use tls::CertResolver;
