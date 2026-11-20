//! HTTP/3: a QUIC endpoint next to the TCP listener of every `http` server
//! with TLS.
//!
//! A [`Listener`] is what one generation serves on one address: routes,
//! limits, log and QUIC TLS settings, shared with its TCP listener. The
//! daemon owns the [`Endpoints`], one UDP socket per address, and they
//! outlive generations: two sockets on one port would split the packets of
//! every connection between them. Installing a generation hands the new
//! connections of each endpoint to its new listener; open connections keep
//! the listener they were accepted under, which drains them when its
//! generation stops.
//!
//! ```text
//!   Endpoints (daemon)        Listener (generation)
//!   :443/udp ---- new connections ----> routes, limits, log, TLS
//!       \
//!        connection ---> one task per request stream ---> Service
//! ```

mod connection;
mod endpoints;
mod listener;

pub(crate) use endpoints::Endpoints;
pub(crate) use listener::Listener;
