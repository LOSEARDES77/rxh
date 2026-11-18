//! Entry points for the fuzz targets in `fuzz/`, enabled by the `fuzzing`
//! feature. Thin wrappers over internal parsers; not a stable API.

use std::net::SocketAddr;

use hyper::Uri;
use rxh_core::config::ProxyProtocol;

use crate::http::cache;
use crate::l4match;
use crate::proxy_protocol;

/// PROXY protocol header announcing `source` to `destination`.
#[must_use]
pub fn proxy_protocol_header(
    version: ProxyProtocol,
    source: SocketAddr,
    destination: SocketAddr,
) -> Vec<u8> {
    proxy_protocol::header(version, source, destination)
}

/// Parses an incoming PROXY protocol header. `Ok(None)` means more bytes
/// are needed, `Ok(Some((source, len)))` carries the client address (`None`
/// keeps the real peer) and the length of the header.
///
/// # Errors
///
/// Fails with a message if `bytes` cannot start a valid header.
pub fn proxy_protocol_parse(
    bytes: &[u8],
) -> Result<Option<(Option<SocketAddr>, usize)>, &'static str> {
    Ok(proxy_protocol::parse(bytes)?.map(|header| (header.source, header.len)))
}

/// Sniffs the protocol of the first bytes of a connection. Returns false if
/// more bytes are needed.
#[must_use]
pub fn l4_detect(bytes: &[u8]) -> bool {
    l4match::detect(bytes).is_some()
}

/// Response cache key of a `GET` request.
#[must_use]
pub fn cache_key(pool: &str, host: &str, uri: &Uri) -> String {
    cache::key(pool, host, uri)
}
