//! Core of RXH: the configuration model and the load balancing engine.
//!
//! This crate has no I/O. It parses and validates configuration files
//! ([`config`]), and keeps the live state of load balancing groups
//! ([`pool`]): members, active and passive health, and the selection
//! policies that Caddy offers (`random`, `round_robin`, `least_conn`,
//! `ip_hash`, `cookie`, ...).

pub mod address;
pub mod config;
pub mod duration;
pub mod matcher;
pub mod net;
pub mod pool;
pub mod size;
pub mod status;
pub mod template;

mod random;

/// RXH version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
