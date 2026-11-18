//! TLS termination: one [`ServerConfig`] per listener, with the certificate
//! picked by SNI among the servers sharing the listener.

use std::fs;
use std::io;
use std::sync::{Arc, RwLock};

use rxh_core::config::{self, Protocol};
use rxh_core::matcher::HostPattern;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::CryptoProvider;
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert};
use tokio_rustls::rustls::sign::CertifiedKey;

/// Picks a certificate by SNI. Exact host names beat wildcards; a client
/// without SNI, or with a name that matches nothing, gets the default (first)
/// certificate.
///
/// Certificates can be added or replaced while the listener runs, see
/// [`CertResolver::set`].
#[derive(Debug)]
pub struct CertResolver {
    provider: Arc<CryptoProvider>,
    // Copy on write: handshakes clone the `Arc` and never wait for a writer.
    certificates: RwLock<Arc<Certificates>>,
}

#[derive(Debug, Default, Clone)]
struct Certificates {
    hosts: Vec<(HostPattern, Arc<CertifiedKey>)>,
    default: Option<Arc<CertifiedKey>>,
}

impl CertResolver {
    /// Empty resolver that parses keys with `provider`.
    #[must_use]
    pub fn new(provider: Arc<CryptoProvider>) -> Self {
        Self {
            provider,
            certificates: RwLock::default(),
        }
    }

    /// Parses a PEM certificate chain and its PEM private key.
    ///
    /// # Errors
    ///
    /// Fails if the PEM is invalid, has no key, or the key does not match.
    pub fn certified_key(&self, cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Arc<CertifiedKey>> {
        let certs = rustls_pemfile::certs(&mut &*cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?;
        if certs.is_empty() {
            return Err(io::Error::other("no certificate found"));
        }
        let key = rustls_pemfile::private_key(&mut &*key_pem)
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("no private key found"))?;
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

    fn select(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
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
        matching(false)
            .or_else(|| matching(true))
            .or_else(|| certificates.default.clone())
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.select(client_hello.server_name())
    }
}

/// Builds the TLS acceptor of a listener shared by `servers`, or `None` if
/// they do not use TLS (validation guarantees all or none of them do). ALPN
/// follows the first server.
pub(crate) fn acceptor(servers: &[&config::Server]) -> io::Result<Option<TlsAcceptor>> {
    let Some(first) = servers.first().filter(|first| first.tls.is_some()) else {
        return Ok(None);
    };

    let builder = ServerConfig::builder().with_no_client_auth();
    let resolver = CertResolver::new(Arc::clone(builder.crypto_provider()));

    for server in servers {
        let Some(tls) = &server.tls else {
            continue;
        };
        let key = resolver
            .certified_key(&fs::read(&tls.cert)?, &fs::read(&tls.key)?)
            .map_err(|err| io::Error::other(format!("{} / {}: {err}", tls.cert, tls.key)))?;
        resolver.set(&server.host, key).map_err(io::Error::other)?;
    }

    let mut config = builder.with_cert_resolver(Arc::new(resolver));
    if first.protocol == Protocol::Http {
        config.alpn_protocols = if first.http2 {
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        } else {
            vec![b"http/1.1".to_vec()]
        };
    }

    Ok(Some(TlsAcceptor::from(Arc::new(config))))
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
