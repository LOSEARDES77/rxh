//! TLS termination: one [`ServerConfig`] per listener, with the certificate
//! picked by SNI among the servers sharing the listener.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use rxh_core::config::{self, Protocol, Tls};
use rxh_core::matcher::HostPattern;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;

/// Picks a certificate by SNI. Exact host names beat wildcards; a client
/// without SNI, or with a name that matches nothing, gets the default (first)
/// certificate, except for names passed to [`CertResolver::reserve`].
///
/// Certificates can be added or replaced while the listener runs, see
/// [`CertResolver::set`].
#[derive(Debug)]
pub struct CertResolver {
    provider: Arc<CryptoProvider>,
    // Copy on write: handshakes clone the `Arc` and never wait for a writer.
    certificates: RwLock<Arc<Certificates>>,
    /// When a failed handshake was last logged, by server name.
    failures_logged: Mutex<HashMap<String, Instant>>,
}

/// Minimum time between two "no certificate" lines for one name.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone)]
struct Certificates {
    hosts: Vec<(HostPattern, Arc<CertifiedKey>)>,
    default: Option<Arc<CertifiedKey>>,
    /// Lowercase names that never get the default certificate.
    reserved: Vec<String>,
}

impl CertResolver {
    /// Empty resolver that parses keys with `provider`.
    #[must_use]
    pub fn new(provider: Arc<CryptoProvider>) -> Self {
        Self {
            provider,
            certificates: RwLock::default(),
            failures_logged: Mutex::default(),
        }
    }

    /// Parses a PEM certificate chain and its PEM private key.
    ///
    /// # Errors
    ///
    /// Fails if the PEM is invalid, has no key, or the key does not match.
    pub fn certified_key(&self, cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Arc<CertifiedKey>> {
        use tokio_rustls::rustls::pki_types::pem::{self, PemObject};
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

        let certs = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        if certs.is_empty() {
            return Err(io::Error::other("no certificate found"));
        }
        let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|err| match err {
            pem::Error::NoItemsFound => io::Error::other("no private key found"),
            err => io::Error::other(err),
        })?;
        let key = CertifiedKey::from_der(certs, key, &self.provider).map_err(io::Error::other)?;
        Ok(Arc::new(key))
    }

    /// Serves `key` for every pattern of `hosts` (exact names or `*.`
    /// wildcards), replacing the certificate a pattern had before. The first
    /// certificate ever set becomes the default.
    ///
    /// # Errors
    ///
    /// Fails if a host pattern is invalid; nothing changes then.
    ///
    /// # Panics
    ///
    /// Panics if the certificate map lock is poisoned, which cannot happen
    /// because nothing panics while holding it.
    pub fn set(&self, hosts: &[String], key: Arc<CertifiedKey>) -> Result<(), String> {
        let patterns: Vec<HostPattern> = hosts
            .iter()
            .map(|host| HostPattern::new(host))
            .collect::<Result<_, _>>()?;

        let mut guard = self
            .certificates
            .write()
            .expect("nothing panics while holding the certificate lock");
        let mut certificates = Certificates::clone(&guard);
        for pattern in patterns {
            match certificates.hosts.iter_mut().find(|(p, _)| *p == pattern) {
                Some((_, existing)) => *existing = Arc::clone(&key),
                None => certificates.hosts.push((pattern, Arc::clone(&key))),
            }
        }
        certificates.default.get_or_insert(key);
        *guard = Arc::new(certificates);
        Ok(())
    }

    /// Makes the handshakes for `names` fail until a certificate covers them,
    /// instead of serving the default certificate. Used for names whose
    /// certificate is still being obtained.
    ///
    /// # Panics
    ///
    /// Panics if the certificate map lock is poisoned, which cannot happen
    /// because nothing panics while holding it.
    pub fn reserve(&self, names: &[String]) {
        let mut guard = self
            .certificates
            .write()
            .expect("nothing panics while holding the certificate lock");
        let mut certificates = Certificates::clone(&guard);
        certificates
            .reserved
            .extend(names.iter().map(|name| name.to_ascii_lowercase()));
        *guard = Arc::new(certificates);
    }

    pub(crate) fn select(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let certificates = Arc::clone(
            &self
                .certificates
                .read()
                .expect("nothing panics while holding the certificate lock"),
        );
        let matching = |wildcard: bool| {
            let name = server_name?;
            certificates
                .hosts
                .iter()
                .find(|(pattern, _)| pattern.is_wildcard() == wildcard && pattern.matches(name))
                .map(|(_, key)| Arc::clone(key))
        };
        let found = matching(false).or_else(|| matching(true));
        if found.is_some() {
            return found;
        }
        if let Some(name) = server_name
            && certificates
                .reserved
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name))
        {
            return None;
        }
        certificates.default.clone()
    }
}

impl CertResolver {
    /// Throttles the failed handshake log to one line per name and
    /// [`FAILURE_LOG_INTERVAL`].
    fn should_log_failure(&self, name: &str, now: Instant) -> bool {
        let mut logged = self
            .failures_logged
            .lock()
            .expect("nothing panics while holding the failure log lock");
        let name = name.to_ascii_lowercase();
        if logged
            .get(&name)
            .is_some_and(|last| now.saturating_duration_since(*last) < FAILURE_LOG_INTERVAL)
        {
            return false;
        }
        // ponytail: forgets everything past 256 names, so random SNI from
        // scanners cannot grow the map; such names log again sooner.
        if logged.len() >= 256 {
            logged.clear();
        }
        logged.insert(name, now);
        true
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let server_name = client_hello.server_name();
        let key = self.select(server_name);
        if key.is_none() {
            let name = server_name.unwrap_or("a client without SNI");
            if self.should_log_failure(name, Instant::now()) {
                eprintln!("TLS => No certificate for {name}, failing the handshake");
            }
        }
        key
    }
}

/// Builds the TLS acceptor of a listener shared by `servers` and the
/// resolver it picks certificates with, or `None` if they do not use TLS
/// (validation guarantees all or none of them do). ALPN follows the first
/// server. `tls = "auto"` servers start without certificates, the ACME
/// manager adds them to the resolver.
pub(crate) fn acceptor(
    servers: &[&config::Server],
) -> io::Result<Option<(TlsAcceptor, Arc<CertResolver>)>> {
    let Some(first) = servers.first().filter(|first| first.tls.is_some()) else {
        return Ok(None);
    };

    let builder = ServerConfig::builder().with_no_client_auth();
    let resolver = Arc::new(CertResolver::new(Arc::clone(builder.crypto_provider())));

    for server in servers {
        match &server.tls {
            None => {}
            Some(Tls::Auto) => resolver.reserve(&server.host),
            Some(Tls::Files { cert, key }) => {
                let certified = resolver
                    .certified_key(&fs::read(cert)?, &fs::read(key)?)
                    .map_err(|err| io::Error::other(format!("{cert} / {key}: {err}")))?;
                resolver
                    .set(&server.host, certified)
                    .map_err(io::Error::other)?;
            }
        }
    }

    let mut config =
        builder.with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
    config.alpn_protocols = match first.protocol {
        Protocol::Http if first.http2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        Protocol::Http => vec![b"http/1.1".to_vec()],
        // A `tcp` server offers the protocols its `alpn` matchers expect, in
        // order, and matches on the one negotiated.
        _ => {
            let mut protocols: Vec<Vec<u8>> = Vec::new();
            for alpn in first.patterns.iter().flat_map(|pattern| &pattern.alpn) {
                if !protocols.iter().any(|known| known == alpn.as_bytes()) {
                    protocols.push(alpn.as_bytes().to_vec());
                }
            }
            protocols
        }
    };

    Ok(Some((TlsAcceptor::from(Arc::new(config)), resolver)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver_with(hosts: &[&[&str]]) -> (CertResolver, Vec<Arc<CertifiedKey>>) {
        let resolver = CertResolver::new(Arc::clone(ServerConfig::builder().crypto_provider()));
        let mut keys = Vec::new();
        for names in hosts {
            let generated =
                rcgen::generate_simple_self_signed(vec![String::from("x.test")]).unwrap();
            let key = resolver
                .certified_key(
                    generated.cert.pem().as_bytes(),
                    generated.signing_key.serialize_pem().as_bytes(),
                )
                .unwrap();
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            resolver.set(&names, Arc::clone(&key)).unwrap();
            keys.push(key);
        }
        (resolver, keys)
    }

    fn is(selected: Option<Arc<CertifiedKey>>, expected: &Arc<CertifiedKey>) -> bool {
        selected.is_some_and(|selected| Arc::ptr_eq(&selected, expected))
    }

    #[test]
    fn exact_beats_wildcard_and_default_is_first() {
        let (resolver, keys) = resolver_with(&[&["*.a.test"], &["www.a.test"], &[]]);
        assert!(is(resolver.select(Some("www.a.test")), &keys[1]));
        assert!(is(resolver.select(Some("api.a.test")), &keys[0]));
        assert!(is(resolver.select(Some("other.test")), &keys[0]));
        assert!(is(resolver.select(None), &keys[0]));
    }

    #[test]
    fn reserved_names_fail_until_set() {
        let (resolver, keys) = resolver_with(&[&["a.test"]]);
        resolver.reserve(&[String::from("Auto.test")]);
        assert!(resolver.select(Some("auto.test")).is_none());
        assert!(is(resolver.select(Some("other.test")), &keys[0]));
        resolver
            .set(&[String::from("auto.test")], Arc::clone(&keys[0]))
            .unwrap();
        assert!(is(resolver.select(Some("AUTO.test")), &keys[0]));
    }

    #[test]
    fn certified_key_rejects_missing_or_mismatched_pem() {
        let resolver = CertResolver::new(Arc::clone(ServerConfig::builder().crypto_provider()));
        let first = rcgen::generate_simple_self_signed(vec![String::from("a.test")]).unwrap();
        let second = rcgen::generate_simple_self_signed(vec![String::from("b.test")]).unwrap();
        let cert = first.cert.pem();
        let key = first.signing_key.serialize_pem();
        assert!(
            resolver
                .certified_key(cert.as_bytes(), key.as_bytes())
                .is_ok()
        );

        let error = |cert: &str, key: &str| {
            resolver
                .certified_key(cert.as_bytes(), key.as_bytes())
                .expect_err("invalid pair")
                .to_string()
        };
        assert_eq!(error("", &key), "no certificate found");
        assert_eq!(error(&key, &key), "no certificate found");
        assert_eq!(error(&cert, ""), "no private key found");
        assert_eq!(error(&cert, &cert), "no private key found");
        let truncated = &cert[..cert.len() / 2];
        assert_ne!(error(truncated, &key), "no certificate found");
        let other_key = second.signing_key.serialize_pem();
        assert!(!error(&cert, &other_key).is_empty(), "the key must match");
    }

    #[test]
    fn failed_handshakes_log_once_per_interval() {
        let (resolver, _) = resolver_with(&[]);
        let start = Instant::now();
        assert!(resolver.should_log_failure("a.test", start));
        assert!(!resolver.should_log_failure("a.test", start + Duration::from_secs(59)));
        assert!(resolver.should_log_failure("b.test", start));
        assert!(resolver.should_log_failure("a.test", start + FAILURE_LOG_INTERVAL));
    }

    #[test]
    fn set_replaces_at_runtime() {
        let (resolver, keys) = resolver_with(&[&["a.test"], &["b.test"]]);
        resolver
            .set(&[String::from("a.test")], Arc::clone(&keys[1]))
            .unwrap();
        assert!(is(resolver.select(Some("a.test")), &keys[1]));
        assert!(
            resolver
                .set(&[String::from("a.*")], Arc::clone(&keys[0]))
                .is_err()
        );
    }
}
