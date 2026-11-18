//! Small in-memory response cache for `GET` requests.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hyper::HeaderMap;
use hyper::Response;
use hyper::StatusCode;
use hyper::Uri;
use hyper::Version;
use hyper::header;
use rxh_core::config;

use crate::http::body;
use crate::http::response::BoxResponse;

#[derive(Debug, Clone)]
struct Entry {
    status: StatusCode,
    version: Version,
    headers: HeaderMap,
    body: Bytes,
    expires_at: Instant,
}

/// Cache key of a `GET` request to `pool`. The pool name and the client
/// controlled host carry their length, so no bytes inside them can make two
/// different requests share an entry (`h|/a` + `/b` against `h` + `/a|/b`).
pub(crate) fn key(pool: &str, host: &str, uri: &Uri) -> String {
    format!("{}:{pool}{}:{host}{uri}", pool.len(), host.len())
}

/// Cache of one route.
#[derive(Debug)]
pub(crate) struct Cache {
    ttl: Duration,
    max_entries: usize,
    max_body_bytes: usize,
    entries: Mutex<HashMap<String, Entry>>,
}

impl Cache {
    /// Returns `None` when the cache is disabled.
    pub(crate) fn new(settings: &config::Cache) -> Option<Self> {
        settings.enabled.then(|| Self {
            ttl: Duration::from_secs(settings.ttl_seconds.max(1)),
            max_entries: settings.max_entries.max(1),
            max_body_bytes: settings.max_body_bytes,
            entries: Mutex::default(),
        })
    }

    /// Largest body worth buffering for the cache.
    pub(crate) fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// Fresh cached response for `key`.
    pub(crate) fn get(&self, key: &str) -> Option<BoxResponse> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = entries.get(key)?;

        if Instant::now() >= entry.expires_at {
            entries.remove(key);
            return None;
        }

        let mut response = Response::new(body::full(entry.body.clone()));
        *response.status_mut() = entry.status;
        *response.version_mut() = entry.version;
        *response.headers_mut() = entry.headers.clone();
        Some(response)
    }

    /// True if a response may be stored, following basic `Cache-Control`.
    pub(crate) fn is_storable(parts: &http::response::Parts) -> bool {
        let no_store = parts
            .headers
            .get_all(header::CACHE_CONTROL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .any(|value| value.contains("no-store") || value.contains("private"));
        parts.status == StatusCode::OK
            && !no_store
            && !parts.headers.contains_key(header::SET_COOKIE)
    }

    /// Stores a response.
    pub(crate) fn put(&self, key: String, parts: &http::response::Parts, body: Bytes) {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();

        if entries.len() >= self.max_entries {
            entries.retain(|_, entry| entry.expires_at > now);
        }
        // ponytail: O(n) eviction of the oldest entry, use an LRU list if
        // caches with many thousands of entries show up in profiles.
        if entries.len() >= self.max_entries
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }

        entries.insert(
            key,
            Entry {
                status: parts.status,
                version: parts.version,
                headers: parts.headers.clone(),
                body,
                expires_at: now + self.ttl,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_keep_pool_host_and_uri_apart() {
        let uri = |text: &str| -> Uri { text.parse().unwrap() };
        assert_ne!(key("p", "h|/a", &uri("/b")), key("p", "h", &uri("/a|/b")));
        assert_ne!(key("|", "", &uri("/")), key("", "|", &uri("/")));
        assert_eq!(key("p", "h", &uri("/a")), key("p", "h", &uri("/a")));
    }
}
