//! TLS termination: one [`ServerConfig`] per listener, with the certificate
//! picked by SNI among the servers sharing the listener.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::mem;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use hyper::header::HeaderValue;
use rxh_core::config::{self, ClientAuth, Protocol, Tls, TlsOptions, TlsVersion};
use rxh_core::matcher::HostPattern;
use serde::Serialize;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::client::danger::HandshakeSignatureValid;
use tokio_rustls::rustls::crypto::{self, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, UnixTime};
use tokio_rustls::rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use tokio_rustls::rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use tokio_rustls::rustls::sign::CertifiedKey;
use tokio_rustls::rustls::{
    self, DigitallySignedStruct, DistinguishedName, RootCertStore, ServerConfig, ServerConnection,
    SignatureScheme, SupportedProtocolVersion,
};

use crate::acme;
use crate::log;
use crate::stream;

/// Picks a certificate by SNI. Exact host names beat wildcards; a client
/// without SNI, or with a name that matches nothing, gets the default (first)
/// certificate, except for names passed to [`CertResolver::reserve`].
///
/// Certificates can be added or replaced while the listener runs, see
/// [`CertResolver::set`]. Certificates loaded from files are read again
/// when the files change, see [`CertResolver::reload_files`].
#[derive(Debug)]
pub struct CertResolver {
    provider: Arc<CryptoProvider>,
    // Copy on write: handshakes clone the `Arc` and never wait for a writer.
    certificates: RwLock<Arc<Certificates>>,
    /// When a failed handshake was last logged, by server name.
    failures_logged: Mutex<HashMap<String, Instant>>,
}

/// Time between two checks of the certificate files for changes.
const FILE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Minimum time between two "no certificate" lines for one name.
const FAILURE_LOG_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Clone)]
struct Certificates {
    hosts: Vec<(HostPattern, Arc<CertifiedKey>)>,
    default: Option<Arc<CertifiedKey>>,
    /// Lowercase names that never get the default certificate.
    reserved: Vec<String>,
    /// Certificates loaded from files, one entry per pair of paths.
    files: Vec<FileCertificate>,
}

#[derive(Debug, Clone)]
struct FileCertificate {
    cert: String,
    key: String,
    /// Hash of the contents of `cert` and `key` when they were last read,
    /// `None` if they could not be. Contents, not modification times, which
    /// miss a rewrite within the same clock tick.
    contents: Option<[u8; 32]>,
    /// What the files gave, as served now (with its OCSP staple).
    served: Arc<CertifiedKey>,
}

/// Contents of a certificate file and its key file.
fn read_pair(cert: &str, key: &str) -> io::Result<(Vec<u8>, Vec<u8>)> {
    Ok((fs::read(cert)?, fs::read(key)?))
}

fn hash_pair(pair: &io::Result<(Vec<u8>, Vec<u8>)>) -> Option<[u8; 32]> {
    use sha2::{Digest, Sha256};

    let (cert, key) = pair.as_ref().ok()?;
    let mut hasher = Sha256::new();
    // The length keeps `ab` + `c` apart from `a` + `bc`.
    hasher.update(cert.len().to_be_bytes());
    hasher.update(cert);
    hasher.update(key);
    Some(hasher.finalize().into())
}

/// A certificate served by a listener, for `rxh cert list`.
#[derive(Debug, Serialize)]
pub(crate) struct CertificateInfo {
    pub(crate) subject: String,
    pub(crate) issuer: String,
    /// DNS names of the subject alternative name extension.
    pub(crate) names: Vec<String>,
    pub(crate) not_before: String,
    pub(crate) not_after: String,
    /// Whole days until `not_after`, negative once expired.
    pub(crate) days_left: i64,
    /// Host patterns this certificate answers for.
    pub(crate) hosts: Vec<String>,
    /// Served to clients whose name matches no host.
    pub(crate) default: bool,
    /// `"file"` or `"acme"`.
    pub(crate) source: &'static str,
    pub(crate) cert_file: Option<String>,
    pub(crate) key_file: Option<String>,
    pub(crate) ocsp_stapled: bool,
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

    pub(crate) fn provider(&self) -> &CryptoProvider {
        &self.provider
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

        self.update(|certificates| {
            for pattern in patterns {
                match certificates.hosts.iter_mut().find(|(p, _)| *p == pattern) {
                    Some((_, existing)) => *existing = Arc::clone(&key),
                    None => certificates.hosts.push((pattern, Arc::clone(&key))),
                }
            }
            certificates.default.get_or_insert(key);
        });
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
        self.update(|certificates| {
            certificates
                .reserved
                .extend(names.iter().map(|name| name.to_ascii_lowercase()));
        });
    }

    /// Serves the certificate of the PEM files `cert` and `key` for `hosts`,
    /// like [`CertResolver::set`], and remembers the files for
    /// [`CertResolver::reload_files`].
    ///
    /// # Errors
    ///
    /// Fails if a file cannot be read or parsed, or a host pattern is
    /// invalid.
    pub(crate) fn load_files(&self, hosts: &[String], cert: &str, key: &str) -> io::Result<()> {
        let known = self
            .snapshot()
            .files
            .iter()
            .find(|file| file.cert == cert && file.key == key)
            .map(|file| Arc::clone(&file.served));
        if let Some(served) = known {
            return self.set(hosts, served).map_err(io::Error::other);
        }
        let pair = read_pair(cert, key);
        let contents = hash_pair(&pair);
        let (cert_pem, key_pem) = pair?;
        let file = FileCertificate {
            cert: cert.to_owned(),
            key: key.to_owned(),
            contents,
            served: self.certified_key(&cert_pem, &key_pem)?,
        };
        self.set(hosts, Arc::clone(&file.served))
            .map_err(io::Error::other)?;
        self.update(|certificates| certificates.files.push(file));
        Ok(())
    }

    /// Reads the certificate files again and serves what they hold now, for
    /// new handshakes. Without `force`, only files whose contents changed since
    /// they were last read. Files that fail to load keep their previous certificate
    /// and are tried again when they change.
    ///
    /// Returns the certificate path of every pair read, with the error if it
    /// failed.
    pub(crate) fn reload_files(&self, force: bool) -> Vec<(String, Result<(), String>)> {
        let mut outcomes = Vec::new();
        for file in &self.snapshot().files {
            let pair = read_pair(&file.cert, &file.key);
            let contents = hash_pair(&pair);
            if !force && contents == file.contents {
                continue;
            }
            let loaded = pair.and_then(|(cert, key)| self.certified_key(&cert, &key));
            self.update(|certificates| {
                let Some(current) = certificates
                    .files
                    .iter_mut()
                    .find(|current| current.cert == file.cert && current.key == file.key)
                else {
                    return;
                };
                current.contents = contents;
                if let Ok(new) = &loaded {
                    let old = mem::replace(&mut current.served, Arc::clone(new));
                    let served = certificates.hosts.iter_mut().map(|(_, key)| key);
                    for key in served.chain(&mut certificates.default) {
                        if Arc::ptr_eq(key, &old) {
                            *key = Arc::clone(new);
                        }
                    }
                }
            });
            let outcome = loaded
                .map(drop)
                .map_err(|err| format!("{} / {}: {err}", file.cert, file.key));
            outcomes.push((file.cert.clone(), outcome));
        }
        outcomes
    }

    /// Every distinct certificate served, described.
    pub(crate) fn certificates(&self) -> Vec<CertificateInfo> {
        let certificates = self.snapshot();
        let mut infos = Vec::new();
        for key in self.keys() {
            let Some(info) = describe(&key) else {
                continue;
            };
            let file = certificates
                .files
                .iter()
                .find(|file| Arc::ptr_eq(&file.served, &key));
            let hosts = certificates
                .hosts
                .iter()
                .filter(|(_, served)| Arc::ptr_eq(served, &key))
                .map(|(pattern, _)| pattern.to_string())
                .collect();
            infos.push(CertificateInfo {
                hosts,
                default: certificates
                    .default
                    .as_ref()
                    .is_some_and(|default| Arc::ptr_eq(default, &key)),
                source: if file.is_some() { "file" } else { "acme" },
                cert_file: file.map(|file| file.cert.clone()),
                key_file: file.map(|file| file.key.clone()),
                ocsp_stapled: key.ocsp.is_some(),
                ..info
            });
        }
        infos
    }

    fn snapshot(&self) -> Arc<Certificates> {
        Arc::clone(
            &self
                .certificates
                .read()
                .expect("nothing panics while holding the certificate lock"),
        )
    }

    /// Applies `change` to a copy of the certificates and publishes it.
    fn update(&self, change: impl FnOnce(&mut Certificates)) {
        let mut guard = self
            .certificates
            .write()
            .expect("nothing panics while holding the certificate lock");
        let mut certificates = Certificates::clone(&guard);
        change(&mut certificates);
        *guard = Arc::new(certificates);
    }

    /// Every distinct certificate served.
    pub(crate) fn keys(&self) -> Vec<Arc<CertifiedKey>> {
        let certificates = self.snapshot();
        let mut keys: Vec<Arc<CertifiedKey>> = Vec::new();
        let served = certificates.hosts.iter().map(|(_, key)| key);
        for key in served.chain(&certificates.default) {
            if !keys.iter().any(|known| Arc::ptr_eq(known, key)) {
                keys.push(Arc::clone(key));
            }
        }
        keys
    }

    /// Serves `new` wherever `old` is served. Does nothing if `old` was
    /// replaced in the meantime.
    pub(crate) fn replace(&self, old: &Arc<CertifiedKey>, new: Arc<CertifiedKey>) {
        self.update(|certificates| {
            let served = certificates.hosts.iter_mut().map(|(_, key)| key);
            let files = certificates.files.iter_mut().map(|file| &mut file.served);
            for key in served.chain(&mut certificates.default).chain(files) {
                if Arc::ptr_eq(key, old) {
                    *key = Arc::clone(&new);
                }
            }
        });
    }

    pub(crate) fn select(&self, server_name: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let certificates = self.snapshot();
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
                .any(|reserved| HostPattern::new(reserved).is_ok_and(|host| host.matches(name)))
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
        // A TLS-ALPN-01 validation only ever sees the challenge certificate,
        // and nothing else ever does (RFC 8737, section 3).
        if client_hello
            .alpn()
            .is_some_and(|mut protocols| protocols.any(|p| p == acme::TLS_ALPN_PROTOCOL))
        {
            return acme::challenge_certificate(server_name?);
        }
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

/// Serves what the certificate files of `resolver` hold whenever they
/// change. Runs until dropped.
pub(crate) async fn watch_files(resolver: Arc<CertResolver>) {
    let mut interval = tokio::time::interval(FILE_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        log_reloads(&resolver.reload_files(/* force= */ false));
    }
}

/// Logs the outcomes of [`CertResolver::reload_files`].
pub(crate) fn log_reloads(outcomes: &[(String, Result<(), String>)]) {
    for (cert, outcome) in outcomes {
        match outcome {
            Ok(()) => println!("TLS => Reloaded {cert}"),
            Err(err) => eprintln!("TLS => {err}, keeping the previous certificate"),
        }
    }
}

/// Subject, issuer, names and validity of the end entity certificate of
/// `key`; the fields that depend on the resolver are left empty.
fn describe(key: &CertifiedKey) -> Option<CertificateInfo> {
    use x509_parser::extensions::GeneralName;

    let (_, certificate) = x509_parser::parse_x509_certificate(key.end_entity_cert().ok()?).ok()?;
    let validity = certificate.validity();
    let time = |time: &x509_parser::time::ASN1Time| {
        let seconds = u64::try_from(time.timestamp()).unwrap_or(0);
        log::rfc3339(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
    };
    let names = certificate
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|extension| {
            let dns = extension.value.general_names.iter();
            dns.filter_map(|name| match name {
                GeneralName::DNSName(name) => Some((*name).to_owned()),
                _ => None,
            })
            .collect()
        })
        .unwrap_or_default();
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    Some(CertificateInfo {
        subject: certificate.subject().to_string(),
        issuer: certificate.issuer().to_string(),
        names,
        not_before: time(&validity.not_before),
        not_after: time(&validity.not_after),
        days_left: (validity.not_after.timestamp() - now).div_euclid(86_400),
        hosts: Vec::new(),
        default: false,
        source: "acme",
        cert_file: None,
        key_file: None,
        ocsp_stapled: false,
    })
}

/// Accepts any client certificate, or none unless `mandatory`: the modes
/// `request` and `require`. The client still has to prove that it holds the
/// key of the certificate it sends.
#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
    mandatory: bool,
}

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        crypto::verify_tls12_signature(message, cert, dss, algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let algorithms = &self.provider.signature_verification_algorithms;
        crypto::verify_tls13_signature(message, cert, dss, algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Client certificate verifier of a `client_auth` table, loading its `ca`
/// files.
fn client_verifier(
    client_auth: &ClientAuth,
    provider: &Arc<CryptoProvider>,
) -> io::Result<Arc<dyn ClientCertVerifier>> {
    let mandatory = client_auth.mode.is_mandatory();
    if !client_auth.mode.verifies() {
        return Ok(Arc::new(AnyClientCert {
            provider: Arc::clone(provider),
            mandatory,
        }));
    }
    let mut roots = RootCertStore::empty();
    for path in &client_auth.ca {
        for cert in stream::read_certs(path)? {
            roots
                .add(cert)
                .map_err(|err| io::Error::other(format!("{path}: {err}")))?;
        }
    }
    let builder = WebPkiClientVerifier::builder_with_provider(roots.into(), Arc::clone(provider));
    let builder = if mandatory {
        builder
    } else {
        builder.allow_unauthenticated()
    };
    builder.build().map_err(io::Error::other)
}

/// Subject of the certificate the client of `connection` presented, such as
/// `CN=alice, O=Example`, as a header value. `None` without a certificate,
/// or if the subject has characters a header cannot carry.
pub(crate) fn client_subject(connection: &ServerConnection) -> Option<HeaderValue> {
    let end_entity = connection.peer_certificates()?.first()?;
    let (_, certificate) = x509_parser::parse_x509_certificate(end_entity).ok()?;
    HeaderValue::from_bytes(certificate.subject().to_string().as_bytes()).ok()
}

/// The crypto provider `default` narrowed to the cipher suites and curves of
/// `options`.
fn provider_with(default: &CryptoProvider, options: &TlsOptions) -> CryptoProvider {
    let mut provider = default.clone();
    if !options.cipher_suites.is_empty() {
        // Rustls calls the TLS 1.3 suites `TLS13_*`, IANA `TLS_*`.
        provider.cipher_suites.retain(|suite| {
            let name = format!("{:?}", suite.suite()).replacen("TLS13_", "TLS_", 1);
            options.cipher_suites.contains(&name)
        });
    }
    if !options.curves.is_empty() {
        provider.kx_groups.retain(|group| {
            let name = format!("{:?}", group.name());
            options
                .curves
                .iter()
                .any(|curve| curve.eq_ignore_ascii_case(&name))
        });
    }
    provider
}

/// Builds the TLS acceptor of a listener shared by `servers` and the
/// resolver it picks certificates with, or `None` if they do not use TLS
/// (validation guarantees all or none of them do). ALPN, `client_auth` and
/// `tls_options` follow the first server. `tls = "auto"` servers start without certificates, the ACME
/// manager adds them to the resolver.
pub(crate) fn acceptor(
    servers: &[&config::Server],
) -> io::Result<Option<(TlsAcceptor, Arc<CertResolver>)>> {
    let Some(first) = servers.first().filter(|first| first.tls.is_some()) else {
        return Ok(None);
    };

    let options = first.tls_options.clone().unwrap_or_default();
    let default = Arc::clone(ServerConfig::builder().crypto_provider());
    let provider = Arc::new(provider_with(&default, &options));
    let versions: &[&SupportedProtocolVersion] = match options.min_version {
        TlsVersion::V1_2 => rustls::DEFAULT_VERSIONS,
        TlsVersion::V1_3 => &[&rustls::version::TLS13],
    };
    let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(versions)
        .map_err(|err| io::Error::other(format!("tls_options: {err}")))?;
    let builder = match &first.client_auth {
        None => builder.with_no_client_auth(),
        Some(client_auth) => {
            builder.with_client_cert_verifier(client_verifier(client_auth, &provider)?)
        }
    };
    let resolver = Arc::new(CertResolver::new(provider));

    for server in servers {
        match &server.tls {
            None => {}
            Some(Tls::Auto) => resolver.reserve(&server.host),
            Some(Tls::Files { cert, key }) => resolver
                .load_files(&server.host, cert, key)
                .map_err(|err| io::Error::other(format!("{cert} / {key}: {err}")))?,
        }
    }

    let mut config =
        builder.with_cert_resolver(Arc::clone(&resolver) as Arc<dyn ResolvesServerCert>);
    let mut alpn_protocols = match first.protocol {
        Protocol::Http if first.http2 => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        Protocol::Http => vec![b"http/1.1".to_vec()],
        // A `tcp` server offers the protocols its `alpn` matchers expect, in
        // order, and matches on the one negotiated.
        _ => {
            let mut protocols: Vec<Vec<u8>> = Vec::new();
            let entries = first.entries();
            for alpn in entries.iter().flat_map(|(_, pattern)| &pattern.alpn) {
                if !protocols.iter().any(|known| known == alpn.as_bytes()) {
                    protocols.push(alpn.as_bytes().to_vec());
                }
            }
            protocols
        }
    };
    // Lets a CA negotiate the TLS-ALPN-01 protocol; the resolver fails such
    // handshakes unless a challenge is pending for the name.
    if servers.iter().any(|server| server.tls == Some(Tls::Auto)) {
        alpn_protocols.push(acme::TLS_ALPN_PROTOCOL.to_vec());
    }
    config.alpn_protocols = alpn_protocols;

    Ok(Some((TlsAcceptor::from(Arc::new(config)), resolver)))
}

/// QUIC flavour of `tcp`, a configuration [`acceptor`] built: the same
/// provider, client verifier and resolver, ALPN `h3` only and no early data.
/// Rustls negotiates nothing but TLS 1.3 on QUIC connections.
pub(crate) fn quic_config(tcp: &ServerConfig) -> Arc<ServerConfig> {
    let mut config = tcp.clone();
    config.alpn_protocols = vec![b"h3".to_vec()];
    config.max_early_data_size = 0;
    Arc::new(config)
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

        // A reserved wildcard covers its subdomains, not the bare domain.
        resolver.reserve(&[String::from("*.wild.test")]);
        assert!(resolver.select(Some("x.Wild.test")).is_none());
        assert!(is(resolver.select(Some("wild.test")), &keys[0]));
        let (wild, _) = resolver_with(&[&["*.wild.test"]]);
        let wild = wild.select(Some("x.wild.test")).unwrap();
        resolver
            .set(&[String::from("*.wild.test")], Arc::clone(&wild))
            .unwrap();
        assert!(is(resolver.select(Some("x.wild.test")), &wild));
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
    fn every_tls_option_name_is_known_to_the_provider() {
        let default = Arc::clone(ServerConfig::builder().crypto_provider());
        for suite in TlsOptions::CIPHER_SUITES {
            let options = TlsOptions {
                cipher_suites: vec![suite.to_owned()],
                ..TlsOptions::default()
            };
            let provider = provider_with(&default, &options);
            assert_eq!(provider.cipher_suites.len(), 1, "{suite}");
        }
        for curve in TlsOptions::CURVES {
            let options = TlsOptions {
                curves: vec![curve.to_ascii_uppercase()],
                ..TlsOptions::default()
            };
            let provider = provider_with(&default, &options);
            assert_eq!(provider.kx_groups.len(), 1, "{curve}");
        }

        let untouched = provider_with(&default, &TlsOptions::default());
        assert_eq!(untouched.cipher_suites.len(), default.cipher_suites.len());
        assert_eq!(untouched.kx_groups.len(), default.kx_groups.len());
    }

    #[test]
    fn replace_swaps_one_certificate_everywhere() {
        let (resolver, keys) = resolver_with(&[&["a.test", "b.test"], &["c.test"]]);
        assert_eq!(resolver.keys().len(), 2, "one entry per certificate");
        let stapled = Arc::new(CertifiedKey {
            ocsp: Some(vec![1]),
            ..CertifiedKey::clone(&keys[0])
        });
        resolver.replace(&keys[0], Arc::clone(&stapled));
        for name in [Some("a.test"), Some("b.test"), None] {
            assert!(is(resolver.select(name), &stapled), "{name:?}");
        }
        assert!(is(resolver.select(Some("c.test")), &keys[1]));
        // The old certificate is gone: replacing it again changes nothing.
        resolver.replace(&keys[0], Arc::clone(&keys[1]));
        assert!(is(resolver.select(Some("a.test")), &stapled));
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

    fn write_pair(dir: &std::path::Path, name: &str) -> (String, String) {
        let generated = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        fs::write(&cert, generated.cert.pem()).unwrap();
        fs::write(&key, generated.signing_key.serialize_pem()).unwrap();
        (
            cert.to_str().unwrap().to_owned(),
            key.to_str().unwrap().to_owned(),
        )
    }

    fn names(resolver: &CertResolver) -> Vec<Vec<String>> {
        let infos = resolver.certificates();
        infos.into_iter().map(|info| info.names).collect()
    }

    #[test]
    fn reload_files_swaps_changed_files_and_keeps_broken_ones_out() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_pair(dir.path(), "one.test");
        let (resolver, _) = resolver_with(&[]);
        let hosts = [String::from("a.test")];
        resolver.load_files(&hosts, &cert, &key).unwrap();
        // The same pair for more names is one certificate, read once.
        resolver
            .load_files(&[String::from("b.test")], &cert, &key)
            .unwrap();
        assert_eq!(resolver.keys().len(), 1);
        assert!(resolver.reload_files(false).is_empty(), "nothing changed");

        // OCSP stapling replaces the key; the files still follow it.
        let served = resolver.select(Some("a.test")).unwrap();
        let stapled = Arc::new(CertifiedKey {
            ocsp: Some(vec![1]),
            ..CertifiedKey::clone(&served)
        });
        resolver.replace(&served, stapled);
        assert!(resolver.certificates()[0].ocsp_stapled);

        write_pair(dir.path(), "two.test");
        let outcomes = resolver.reload_files(false);
        assert_eq!(outcomes, [(cert.clone(), Ok(()))]);
        assert_eq!(names(&resolver), [[String::from("two.test")]]);
        let info = &resolver.certificates()[0];
        assert_eq!(info.hosts, ["a.test", "b.test"]);
        assert!(info.default);
        assert!(!info.ocsp_stapled, "a new certificate has no staple yet");
        assert_eq!(
            (info.source, info.cert_file.as_deref()),
            ("file", Some(cert.as_str()))
        );

        // A broken file is reported once, then left alone until it changes.
        fs::write(&key, "garbage").unwrap();
        let outcomes = resolver.reload_files(false);
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].1.as_ref().unwrap_err().contains("key.pem"));
        assert!(resolver.reload_files(false).is_empty());
        assert_eq!(names(&resolver), [[String::from("two.test")]]);
        assert_eq!(resolver.reload_files(true).len(), 1, "force reads it again");

        write_pair(dir.path(), "three.test");
        assert!(resolver.reload_files(false)[0].1.is_ok());
        assert!(resolver.select(Some("b.test")).is_some());
        assert_eq!(names(&resolver), [[String::from("three.test")]]);
    }

    #[test]
    fn certificates_describe_acme_ones_and_expiry() {
        let mut params = rcgen::CertificateParams::new(vec![String::from("old.test")]).unwrap();
        params.not_after = rcgen::date_time_ymd(2001, 1, 1);
        let signing = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&signing).unwrap();
        let resolver = CertResolver::new(Arc::clone(ServerConfig::builder().crypto_provider()));
        let key = resolver
            .certified_key(cert.pem().as_bytes(), signing.serialize_pem().as_bytes())
            .unwrap();
        resolver.set(&[String::from("*.old.test")], key).unwrap();

        let info = &resolver.certificates()[0];
        assert_eq!(info.source, "acme");
        assert_eq!(info.cert_file, None);
        assert_eq!(info.hosts, ["*.old.test"]);
        assert_eq!(info.names, ["old.test"]);
        assert_eq!(info.not_after, "2001-01-01T00:00:00.000Z");
        assert!(info.days_left < -9000, "{}", info.days_left);
    }
}
