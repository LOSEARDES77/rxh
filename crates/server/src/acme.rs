//! Automatic HTTPS: obtains and renews the certificates of `tls = "auto"`
//! servers with ACME HTTP-01, TLS-ALPN-01 or DNS-01 (`[acme] challenge`) and
//! swaps them into the TLS listeners.
//!
//! Files under `[acme] storage`, all written with mode 0600 in 0700
//! directories on unix:
//!
//! ```text
//! accounts/<CA>.json        account credentials
//! <CA>/<domain>/cert.pem    certificate chain
//! <CA>/<domain>/key.pem     private key
//! ```
//!
//! `<CA>` is the `ca` URL without `https://`, every character other than
//! letters, digits, `.` and `-` replaced by `_`. `<domain>` of a wildcard
//! `*.example.com` is `wildcard_.example.com`. Certificates are kept per CA,
//! so switching `ca` (staging to production) obtains new ones instead of
//! serving the old CA's.
//!
//! On start the stored certificates go into the resolvers right away. A
//! background task then obtains the missing ones and the ones expiring
//! within `renew_before`, and checks again every day.
//!
//! Issuance state is process wide, so it survives reloads: an order runs in
//! a detached task (with a deadline) that a reload does not abort, a name is
//! never ordered twice at once, and a name whose order failed at a CA waits
//! [`RETRY_INTERVAL`] before the next one there, whichever generation asks.
//! Rounds that stop before contacting the CA (port problems) start no
//! backoff. The generation that did not order a certificate picks it up from
//! the storage. The challenge-only listener is process wide too; a reload
//! that puts a server on its port closes it first.
//!
//! TLS-ALPN-01 needs no port of its own: the TLS listeners of the
//! `tls = "auto"` servers answer a handshake with ALPN `acme-tls/1` with the
//! pending challenge certificate of the name and close the connection.
//!
//! DNS-01 needs no port either: the TXT record `_acme-challenge.NAME` is
//! created at the DNS provider of `[acme.dns]` (see [`crate::dns01`]), the
//! order waits `propagation_wait` and the record is deleted when the
//! validation ends, however it ends. It is the only challenge for wildcard
//! names. DNS-01 orders of the whole process run one at a time, because a
//! wildcard and its base name are validated at the same record name.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::io;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime};

use hyper::header;
use hyper::service::service_fn;
use hyper::{Method, StatusCode};
use hyper_util::rt::TokioIo;
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rxh_core::config::{self, AcmeChallenge, AcmeDns, Config, Protocol, Tls};
use tokio::net::{TcpListener, TcpSocket};
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::rustls::pki_types::PrivateKeyDer;
use tokio_rustls::rustls::sign::CertifiedKey;

use crate::dns01;
use crate::http::body;
use crate::http::response::{self, BoxResponse};
use crate::master::Master;
use crate::tls::CertResolver;

/// Path prefix of HTTP-01 challenge requests.
const CHALLENGE_PREFIX: &str = "/.well-known/acme-challenge/";

/// Time between two renewal checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Time before retrying a name after a failed issuance, well under the CA
/// rate limits for failed validations.
const RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Time between checks while another generation's order for a name runs.
const IN_FLIGHT_POLL: Duration = Duration::from_secs(10);

/// Upper bound for loading or registering the account.
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(60);

/// Upper bound for one issuance, network included.
const ISSUE_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Pending HTTP-01 tokens and their key authorizations. Process wide, so
/// every listener of every generation answers them; tokens are unique.
static CHALLENGES: LazyLock<RwLock<HashMap<String, String>>> = LazyLock::new(RwLock::default);

/// ALPN protocol of a TLS-ALPN-01 validation handshake (RFC 8737).
pub(crate) const TLS_ALPN_PROTOCOL: &[u8] = b"acme-tls/1";

/// Pending TLS-ALPN-01 challenge certificates by lowercase name. Process
/// wide, like [`CHALLENGES`].
static TLS_CHALLENGES: LazyLock<RwLock<HashMap<String, Arc<CertifiedKey>>>> =
    LazyLock::new(RwLock::default);

/// Held by the DNS-01 order that has records published.
static DNS_ORDER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Orders in flight and recent failures, by name, for every generation.
static ISSUANCE: LazyLock<Mutex<Issuance>> = LazyLock::new(Mutex::default);

/// The challenge-only listener, shared by the rounds of every generation.
static CHALLENGE_LISTENER: LazyLock<Mutex<Weak<ChallengeListener>>> = LazyLock::new(Mutex::default);

/// Answers `GET /.well-known/acme-challenge/TOKEN` for a pending token, or
/// returns `None` to let the request be routed normally.
pub(crate) fn challenge_response(method: &Method, path: &str) -> Option<BoxResponse> {
    if method != Method::GET {
        return None;
    }
    let token = path.strip_prefix(CHALLENGE_PREFIX)?;
    let key_authorization = CHALLENGES
        .read()
        .expect("nothing panics while holding the challenge lock")
        .get(token)?
        .clone();
    let mut response = hyper::Response::new(body::full(key_authorization));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/plain"),
    );
    Some(response)
}

/// The certificate a TLS-ALPN-01 validation handshake for `name` gets, if a
/// challenge is pending for it.
pub(crate) fn challenge_certificate(name: &str) -> Option<Arc<CertifiedKey>> {
    TLS_CHALLENGES
        .read()
        .expect("nothing panics while holding the challenge lock")
        .get(&name.to_ascii_lowercase())
        .cloned()
}

/// Self-signed TLS-ALPN-01 certificate for `name`: the name as its only
/// SAN and the critical `acmeIdentifier` extension holding `digest`, the
/// SHA-256 of the key authorization (RFC 8737, section 3).
fn challenge_certified_key(
    domain: &Domain,
    digest: &[u8],
) -> Result<Arc<CertifiedKey>, Box<dyn std::error::Error>> {
    let mut params = rcgen::CertificateParams::new(vec![domain.name.clone()])?;
    params.custom_extensions = vec![rcgen::CustomExtension::new_acme_identifier(digest)];
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    let resolver = domain.resolvers.first().ok_or("the name has no listener")?;
    // Not `CertResolver::certified_key`: its check that key and certificate
    // match rejects the critical extension.
    let key = PrivateKeyDer::try_from(key.serialize_der())?;
    let key = resolver.provider().key_provider.load_private_key(key)?;
    Ok(Arc::new(CertifiedKey::new(vec![cert.der().clone()], key)))
}

/// Challenge answers published while an order runs, withdrawn when dropped.
#[derive(Default)]
struct Published {
    /// HTTP-01 tokens.
    tokens: Vec<String>,
    /// Names with a TLS-ALPN-01 certificate.
    names: Vec<String>,
    /// DNS-01 TXT records and the provider settings they were created with.
    records: Vec<(AcmeDns, dns01::Record)>,
}

impl Published {
    fn add(&mut self, token: String, key_authorization: String) {
        CHALLENGES
            .write()
            .expect("nothing panics while holding the challenge lock")
            .insert(token.clone(), key_authorization);
        self.tokens.push(token);
    }

    fn add_certificate(&mut self, name: &str, certificate: Arc<CertifiedKey>) {
        TLS_CHALLENGES
            .write()
            .expect("nothing panics while holding the challenge lock")
            .insert(name.to_owned(), certificate);
        self.names.push(name.to_owned());
    }

    /// Deletes the DNS-01 records now. What is left when the order is
    /// cancelled is deleted by a task started on drop.
    async fn withdraw_records(&mut self) {
        for (dns, record) in self.records.drain(..) {
            if let Err(err) = dns01::withdraw(&dns, &record).await {
                eprintln!(
                    "ACME => Deleting a DNS-01 record at {}: {err}",
                    dns.provider
                );
            }
        }
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        if !self.tokens.is_empty() {
            let mut challenges = CHALLENGES
                .write()
                .expect("nothing panics while holding the challenge lock");
            for token in &self.tokens {
                challenges.remove(token);
            }
        }
        if !self.names.is_empty() {
            let mut challenges = TLS_CHALLENGES
                .write()
                .expect("nothing panics while holding the challenge lock");
            for name in &self.names {
                challenges.remove(name);
            }
        }
        if !self.records.is_empty()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let mut rest = Self::default();
            rest.records = std::mem::take(&mut self.records);
            runtime.spawn(async move { rest.withdraw_records().await });
        }
    }
}

/// Issuance history of the names, see the module documentation.
#[derive(Debug, Default)]
struct Issuance {
    names: HashMap<String, Attempt>,
}

#[derive(Debug, Default)]
struct Attempt {
    /// Per name, whatever the CA: two orders would race on the same files.
    in_flight: bool,
    /// CA directory URL and time of the last failed order. The backoff only
    /// applies to that CA, so switching `ca` retries at once.
    failed: Option<(String, Instant)>,
}

/// Answer of [`Issuance::claim`].
#[derive(Debug, PartialEq, Eq)]
enum Claim {
    /// The caller orders the certificate and must call
    /// [`Issuance::finish`].
    Granted,

    /// Another order for the name is running.
    InFlight,

    /// The last order at this CA failed; the next one may start after this
    /// time.
    BackingOff(Duration),
}

/// How a granted claim ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Certificate installed: clears the backoff.
    Succeeded,

    /// The CA was contacted and the order failed: starts the backoff.
    Failed,

    /// Stopped before contacting the CA (configuration or local port
    /// problem): leaves the backoff as it was.
    NotAttempted,
}

impl Issuance {
    fn claim(&mut self, name: &str, ca: &str, now: Instant) -> Claim {
        let attempt = self.names.entry(name.to_owned()).or_default();
        if attempt.in_flight {
            return Claim::InFlight;
        }
        if let Some((failed_ca, failed_at)) = &attempt.failed
            && failed_ca == ca
        {
            let elapsed = now.saturating_duration_since(*failed_at);
            if elapsed < RETRY_INTERVAL {
                return Claim::BackingOff(RETRY_INTERVAL - elapsed);
            }
        }
        attempt.in_flight = true;
        Claim::Granted
    }

    fn finish(&mut self, name: &str, ca: &str, outcome: Outcome, now: Instant) {
        let attempt = self.names.entry(name.to_owned()).or_default();
        attempt.in_flight = false;
        match outcome {
            Outcome::Succeeded => attempt.failed = None,
            Outcome::Failed => attempt.failed = Some((ca.to_owned(), now)),
            Outcome::NotAttempted => {}
        }
    }
}

/// A granted claim on the global [`ISSUANCE`]; finishes it when dropped
/// with `outcome`, a failure unless changed.
struct ClaimGuard {
    name: String,
    ca: String,
    outcome: Outcome,
}

impl ClaimGuard {
    fn set_outcome(&mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        ISSUANCE
            .lock()
            .expect("nothing panics while holding the issuance lock")
            .finish(&self.name, &self.ca, self.outcome, Instant::now());
    }
}

/// Certificate files of `[acme] storage` for one CA, see the module
/// documentation.
#[derive(Debug, Clone)]
struct Storage {
    root: PathBuf,
    /// `<CA>` of the module documentation.
    ca_name: String,
}

impl Storage {
    fn new(root: PathBuf, ca: &str) -> Self {
        let ca_name = ca
            .trim_start_matches("https://")
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self { root, ca_name }
    }

    /// Directory of `domain`, refusing names that are not domain names so a
    /// name can never escape `root`. A wildcard gets a name that every file
    /// system accepts.
    fn domain_dir(&self, domain: &str) -> io::Result<PathBuf> {
        let (base, dir) = match domain.strip_prefix("*.") {
            Some(base) => (base, format!("wildcard_.{base}")),
            None => (domain, domain.to_owned()),
        };
        config::validate_domain(base).map_err(io::Error::other)?;
        Ok(self.root.join(&self.ca_name).join(dir))
    }

    fn account_path(&self) -> PathBuf {
        self.root
            .join("accounts")
            .join(format!("{}.json", self.ca_name))
    }

    /// Certificate chain and key of `domain`, if both files exist.
    fn load(&self, domain: &str) -> io::Result<(Vec<u8>, Vec<u8>)> {
        let dir = self.domain_dir(domain)?;
        Ok((
            fs::read(dir.join("cert.pem"))?,
            fs::read(dir.join("key.pem"))?,
        ))
    }

    fn save(&self, domain: &str, cert_pem: &[u8], key_pem: &[u8]) -> io::Result<()> {
        let dir = self.domain_dir(domain)?;
        write_private(&dir.join("key.pem"), key_pem)?;
        write_private(&dir.join("cert.pem"), cert_pem)
    }
}

/// Writes `contents` through a temporary file and a rename, creating the
/// parent directories. Owner only permissions on unix. The temporary name is
/// unique, so concurrent writers never mix their contents.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or(Path::new("."));
    let mut dirs = fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut dirs, 0o700);
    dirs.create(parent)?;

    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{}.{unique}.tmp", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&temporary).and_then(|mut file| {
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    });
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    written
}

/// Expiry of the first certificate of a PEM chain.
fn not_after(cert_pem: &[u8]) -> Option<SystemTime> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem).ok()?;
    let certificate = pem.parse_x509().ok()?;
    let seconds = u64::try_from(certificate.validity().not_after.timestamp()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

/// True if a certificate must be obtained: there is none, it cannot be
/// parsed, or it expires within `renew_before` of `now`.
fn renewal_due(cert_pem: Option<&[u8]>, now: SystemTime, renew_before: Duration) -> bool {
    match cert_pem.and_then(not_after) {
        Some(expiry) => expiry <= now + renew_before,
        None => true,
    }
}

/// One certified name and the listeners that serve it.
#[derive(Clone)]
struct Domain {
    name: String,
    resolvers: Vec<Arc<CertResolver>>,
}

impl Domain {
    /// True if every listener has a certificate for the name.
    fn served(&self) -> bool {
        self.resolvers
            .iter()
            .all(|resolver| resolver.select(Some(&self.name)).is_some())
    }
}

/// Who answers the challenges: for HTTP-01, whoever listens on `http_port`.
/// DNS-01 is answered by the DNS provider, with no listener at all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChallengePort {
    /// A listener of the configuration: a plain `http` server for HTTP-01,
    /// the TLS listeners of the `tls = "auto"` servers for TLS-ALPN-01.
    Served,

    /// Nothing of the configuration uses the port: a challenge-only
    /// listener on this address while issuing.
    Listener(SocketAddr),

    /// A server that cannot answer (TLS or `tcp`) uses the port.
    Blocked(String),
}

impl ChallengePort {
    fn of(config: &Config) -> Self {
        if config.acme.challenge != AcmeChallenge::Http01 {
            return Self::Served;
        }
        let port = config.acme.http_port;
        let mut users = config.servers.iter().enumerate().filter(|(_, server)| {
            server.protocol != Protocol::Udp
                && server.listen.iter().any(|address| address.port() == port)
        });
        let mut blocker = None;
        for (index, server) in users.by_ref() {
            if server.protocol == Protocol::Http && server.tls.is_none() {
                return Self::Served;
            }
            blocker.get_or_insert((index, server));
        }
        match blocker {
            None => Self::Listener(SocketAddr::from(([0, 0, 0, 0], port))),
            Some((index, server)) => Self::Blocked(format!(
                "port {port} is used by {} ({}{}), which cannot answer HTTP-01 challenges; \
                 add a plain http server on it or change [acme] http_port",
                server.label(index),
                server.protocol,
                if server.tls.is_some() {
                    " with TLS"
                } else {
                    ""
                },
            )),
        }
    }
}

/// Obtains and renews the certificates of one configuration.
#[derive(Clone)]
pub(crate) struct Manager {
    settings: config::Acme,
    ca_root: Option<PathBuf>,
    storage: Storage,
    domains: Vec<Domain>,
    challenge_port: ChallengePort,
    /// Names whose backoff was already logged, to log it once.
    backoff_logged: Arc<Mutex<HashSet<String>>>,
}

impl Manager {
    /// Manager for the `tls = "auto"` servers of `config`, or `None` if there
    /// are none. Relative `storage` and `ca_root` paths are joined to `base`.
    /// Stored certificates are installed into the listeners of `master`
    /// before this returns.
    pub(crate) fn new(config: &Config, master: &Master, base: &Path) -> Option<Self> {
        let mut domains: Vec<Domain> = Vec::new();
        for (index, server) in config.servers.iter().enumerate() {
            if server.tls != Some(Tls::Auto) {
                continue;
            }
            let resolvers = master.cert_resolvers(index);
            for host in &server.host {
                let name = host.to_ascii_lowercase();
                match domains.iter_mut().find(|domain| domain.name == name) {
                    Some(domain) => domain.resolvers.extend(resolvers.iter().cloned()),
                    None => domains.push(Domain {
                        name,
                        resolvers: resolvers.clone(),
                    }),
                }
            }
        }
        if domains.is_empty() {
            return None;
        }

        let settings = config.acme.clone();
        let manager = Self {
            ca_root: settings.ca_root.as_ref().map(|root| base.join(root)),
            storage: Storage::new(base.join(&settings.storage), &settings.ca),
            challenge_port: ChallengePort::of(config),
            backoff_logged: Arc::default(),
            settings,
            domains,
        };

        for domain in &manager.domains {
            if let Err(err) = manager.load_stored(domain)
                && err.kind() != io::ErrorKind::NotFound
            {
                eprintln!("ACME => Stored certificate of {}: {err}", domain.name);
            }
        }
        Some(manager)
    }

    /// Installs the stored certificate of `domain`.
    fn load_stored(&self, domain: &Domain) -> io::Result<()> {
        let (cert, key) = self.storage.load(&domain.name)?;
        install(domain, &cert, &key).map_err(io::Error::other)
    }

    /// Obtains missing and expiring certificates now, then checks daily.
    /// Runs until aborted; an order already started finishes anyway.
    pub(crate) async fn run(self) {
        loop {
            let wait = self.check().await;
            tokio::time::sleep(wait).await;
        }
    }

    /// One renewal check. Returns the time until the next one.
    async fn check(&self) -> Duration {
        let now = SystemTime::now();
        let mut wait = CHECK_INTERVAL;
        let mut due = Vec::new();
        for domain in &self.domains {
            if !domain.served() {
                // Possibly ordered by another generation in the meantime.
                let _ = self.load_stored(domain);
            }
            let cert = self.storage.load(&domain.name).ok();
            // A stored pair that does not install is replaced too.
            let needed = !domain.served()
                || renewal_due(
                    cert.as_ref().map(|(cert, _)| cert.as_slice()),
                    now,
                    self.settings.renew_before,
                );
            if !needed {
                continue;
            }
            let claim = ISSUANCE
                .lock()
                .expect("nothing panics while holding the issuance lock")
                .claim(&domain.name, &self.settings.ca, Instant::now());
            let mut backoff_logged = self
                .backoff_logged
                .lock()
                .expect("nothing panics while holding the backoff log lock");
            match claim {
                Claim::Granted => {
                    backoff_logged.remove(&domain.name);
                    due.push((
                        domain.clone(),
                        ClaimGuard {
                            name: domain.name.clone(),
                            ca: self.settings.ca.clone(),
                            outcome: Outcome::Failed,
                        },
                    ));
                }
                Claim::InFlight => wait = wait.min(IN_FLIGHT_POLL),
                Claim::BackingOff(left) => {
                    if backoff_logged.insert(domain.name.clone()) {
                        println!(
                            "ACME => Next order for {} in {}s, after a failed one at {}",
                            domain.name,
                            left.as_secs(),
                            self.settings.ca
                        );
                    }
                    wait = wait.min(left);
                }
            }
        }
        if due.is_empty() {
            return wait;
        }

        // Detached: a reload aborts this task, not the orders. The deadline
        // frees the names and the challenge port even if the CA stalls.
        let count = u32::try_from(due.len()).unwrap_or(u32::MAX);
        let deadline = ACCOUNT_TIMEOUT + self.issue_timeout().saturating_mul(count);
        let round = self.clone().issue_round(due);
        let round = tokio::task::spawn(async move {
            let result = tokio::time::timeout(deadline, round).await;
            result.unwrap_or_else(|_| {
                eprintln!("ACME => Issuance stopped after {deadline:?}");
                false
            })
        });
        match round.await {
            Ok(true) => wait,
            _ => wait.min(RETRY_INTERVAL),
        }
    }

    /// Issues a certificate for every domain of `due`. Returns false if any
    /// failed.
    async fn issue_round(self, mut due: Vec<(Domain, ClaimGuard)>) -> bool {
        // Local problems are not the CA's: no backoff, so a fixed
        // configuration is retried as soon as it is reloaded.
        let mut not_attempted = |message: String| {
            eprintln!("ACME => {message}; retrying in {RETRY_INTERVAL:?} or on reload");
            for (_, guard) in &mut due {
                guard.set_outcome(Outcome::NotAttempted);
            }
        };
        let _listener = match &self.challenge_port {
            ChallengePort::Served => None,
            ChallengePort::Listener(address) => {
                match ChallengeListener::acquire_soon(*address).await {
                    Ok(listener) => Some(listener),
                    Err(err) => {
                        not_attempted(format!(
                            "Cannot answer HTTP-01 challenges on {address}: {err}"
                        ));
                        return false;
                    }
                }
            }
            ChallengePort::Blocked(why) => {
                not_attempted(format!("Cannot obtain certificates: {why}"));
                return false;
            }
        };
        let account = match tokio::time::timeout(ACCOUNT_TIMEOUT, self.account()).await {
            Ok(Ok(account)) => account,
            Ok(Err(err)) => {
                eprintln!("ACME => Account at {}: {err}", self.settings.ca);
                return false;
            }
            Err(_) => {
                eprintln!("ACME => Account at {}: timed out", self.settings.ca);
                return false;
            }
        };

        let mut ok = true;
        for (domain, mut guard) in due {
            println!("ACME => Obtaining a certificate for {}", domain.name);
            let order = issue(&account, &domain, &self.settings);
            let result = match tokio::time::timeout(self.issue_timeout(), order).await {
                Ok(result) => result,
                Err(_) => Err(String::from("timed out")),
            };
            let installed = result.and_then(|(cert, key)| {
                self.storage
                    .save(&domain.name, cert.as_bytes(), key.as_bytes())
                    .map_err(|err| format!("storing in {}: {err}", self.storage.root.display()))?;
                install(&domain, cert.as_bytes(), key.as_bytes())?;
                Ok(cert)
            });
            match installed {
                Ok(cert) => {
                    guard.set_outcome(Outcome::Succeeded);
                    println!("ACME => Certificate for {} installed", domain.name);
                    let renew_before = self.settings.renew_before;
                    if renewal_due(Some(cert.as_bytes()), SystemTime::now(), renew_before) {
                        eprintln!(
                            "ACME => The new certificate of {} is already within renew_before \
                             ({renew_before:?}) of its expiry, it will be renewed at every \
                             daily check; lower [acme] renew_before",
                            domain.name
                        );
                    }
                }
                Err(err) => {
                    eprintln!("ACME => Certificate for {} failed: {err}", domain.name);
                    ok = false;
                }
            }
        }
        ok
    }

    /// Upper bound for one order: [`ISSUE_TIMEOUT`] plus the DNS-01
    /// propagation wait, which is not the CA's time.
    fn issue_timeout(&self) -> Duration {
        match (&self.settings.dns, self.settings.challenge) {
            (Some(dns), AcmeChallenge::Dns01) => ISSUE_TIMEOUT + dns.propagation_wait,
            _ => ISSUE_TIMEOUT,
        }
    }

    /// Loads the stored account for the CA, or registers a new one (agreeing
    /// to the CA's terms of service) and stores it.
    async fn account(&self) -> Result<Account, String> {
        let builder = || {
            match &self.ca_root {
                Some(root) => Account::builder_with_root(root),
                None => Account::builder(),
            }
            .map_err(|err| err.to_string())
        };
        let path = self.storage.account_path();
        match fs::read_to_string(&path) {
            Ok(text) => {
                let credentials: AccountCredentials = serde_json::from_str(&text)
                    .map_err(|err| format!("{}: {err}", path.display()))?;
                return builder()?
                    .from_credentials(credentials)
                    .await
                    .map_err(|err| err.to_string());
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display())),
        }

        let contact = self
            .settings
            .email
            .as_ref()
            .map(|email| format!("mailto:{email}"));
        let contacts: Vec<&str> = contact.iter().map(String::as_str).collect();
        let new_account = NewAccount {
            contact: &contacts,
            terms_of_service_agreed: true,
            only_return_existing: false,
        };
        let (account, credentials) = builder()?
            .create(&new_account, self.settings.ca.clone(), None)
            .await
            .map_err(|err| err.to_string())?;
        let json = serde_json::to_vec(&credentials).map_err(|err| err.to_string())?;
        write_private(&path, &json).map_err(|err| format!("{}: {err}", path.display()))?;
        println!("ACME => Registered an account at {}", self.settings.ca);
        Ok(account)
    }
}

/// Parses `cert` and `key` and serves them for `domain` on its listeners.
fn install(domain: &Domain, cert: &[u8], key: &[u8]) -> Result<(), String> {
    for resolver in &domain.resolvers {
        let certified = resolver
            .certified_key(cert, key)
            .map_err(|err| err.to_string())?;
        resolver.set(std::slice::from_ref(&domain.name), certified)?;
    }
    Ok(())
}

/// Runs one order for `domain`, validated with the challenge of `settings`.
/// Returns the PEM certificate chain and the PEM private key.
async fn issue(
    account: &Account,
    domain: &Domain,
    settings: &config::Acme,
) -> Result<(String, String), String> {
    let error = |err: instant_acme::Error| err.to_string();
    let challenge = settings.challenge;
    let _one_dns_order = match challenge {
        AcmeChallenge::Dns01 => Some(DNS_ORDER.lock().await),
        AcmeChallenge::Http01 | AcmeChallenge::TlsAlpn01 => None,
    };
    let identifiers = [Identifier::Dns(domain.name.clone())];
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(error)?;

    let mut published = Published::default();
    let mut authorizations = order.authorizations();
    while let Some(authorization) = authorizations.next().await {
        let mut authorization = authorization.map_err(error)?;
        match authorization.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            status => return Err(format!("authorization is {status:?}")),
        }
        let mut offered = match challenge {
            AcmeChallenge::Http01 => {
                let offered = authorization
                    .challenge(ChallengeType::Http01)
                    .ok_or("the CA offers no http-01 challenge")?;
                published.add(
                    offered.token.clone(),
                    offered.key_authorization().as_str().to_owned(),
                );
                offered
            }
            AcmeChallenge::TlsAlpn01 => {
                let offered = authorization
                    .challenge(ChallengeType::TlsAlpn01)
                    .ok_or("the CA offers no tls-alpn-01 challenge")?;
                let digest = offered.key_authorization().digest();
                let certificate = challenge_certified_key(domain, digest.as_ref())
                    .map_err(|err| format!("challenge certificate: {err}"))?;
                published.add_certificate(&domain.name, certificate);
                offered
            }
            AcmeChallenge::Dns01 => {
                let dns = settings.dns.as_ref().ok_or("[acme.dns] is missing")?;
                let offered = authorization
                    .challenge(ChallengeType::Dns01)
                    .ok_or("the CA offers no dns-01 challenge")?;
                let name = format!("_acme-challenge.{}", domain.name.trim_start_matches("*."));
                let value = offered.key_authorization().dns_value();
                let record = dns01::publish(dns, &name, &value)
                    .await
                    .map_err(|err| format!("TXT record {name} at {}: {err}", dns.provider))?;
                published.records.push((dns.clone(), record));
                println!(
                    "ACME => TXT record {name} created at {}, validating in {:?}",
                    dns.provider, dns.propagation_wait
                );
                tokio::time::sleep(dns.propagation_wait).await;
                offered
            }
        };
        offered.set_ready().await.map_err(error)?;
    }

    let retries = RetryPolicy::new().timeout(Duration::from_secs(120));
    let status = order.poll_ready(&retries).await;
    published.withdraw_records().await;
    drop(published);
    let status = status.map_err(error)?;
    if status != OrderStatus::Ready {
        return Err(format!("order is {status:?} after validation"));
    }
    let key = order.finalize().await.map_err(error)?;
    let cert = order.poll_certificate(&retries).await.map_err(error)?;
    Ok((cert, key))
}

/// Answers HTTP-01 challenges on `0.0.0.0:http_port` while a round holds it.
/// Bound without `SO_REUSEPORT`, so it never shares the port with another
/// socket: a conflict is an error instead of stolen connections.
struct ChallengeListener {
    /// Requested address, to share the listener between rounds.
    address: SocketAddr,
    /// Bound port, to release it for a new configuration.
    port: u16,
    /// Taken by [`release_challenge_port`] to close the socket early.
    task: Mutex<Option<JoinHandle<()>>>,
}

impl ChallengeListener {
    /// The listener on `address`, opened unless a running round already
    /// holds it.
    fn acquire(address: SocketAddr) -> io::Result<Arc<Self>> {
        let mut current = CHALLENGE_LISTENER
            .lock()
            .expect("nothing panics while holding the challenge listener lock");
        if let Some(listener) = current.upgrade()
            && listener.address == address
        {
            return Ok(listener);
        }
        let socket = bind_exclusive(address)?;
        let listener = Arc::new(Self {
            address,
            port: socket.local_addr()?.port(),
            task: Mutex::new(Some(tokio::task::spawn(serve_challenges(socket)))),
        });
        println!("ACME => Answering HTTP-01 challenges on {address}");
        *current = Arc::downgrade(&listener);
        Ok(listener)
    }

    /// [`ChallengeListener::acquire`], retrying for a moment while a just
    /// dropped listener's socket is still closing.
    async fn acquire_soon(address: SocketAddr) -> io::Result<Arc<Self>> {
        for _ in 0..10 {
            match Self::acquire(address) {
                Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                result => return result,
            }
        }
        Self::acquire(address)
    }
}

impl Drop for ChallengeListener {
    fn drop(&mut self) {
        let task = self
            .task
            .get_mut()
            .expect("nothing panics while holding the challenge task lock");
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

/// Closes the challenge-only listener now if a server of `config` listens on
/// its port, so the new generation can bind it. The round that opened it
/// keeps going: the new `http` servers answer its tokens, which are process
/// wide.
pub(crate) async fn release_challenge_port(config: &Config) {
    let ports: HashSet<u16> = config
        .servers
        .iter()
        .filter(|server| server.protocol != Protocol::Udp)
        .flat_map(|server| server.listen.iter().map(SocketAddr::port))
        .collect();
    let current = CHALLENGE_LISTENER
        .lock()
        .expect("nothing panics while holding the challenge listener lock")
        .upgrade();
    if let Some(listener) = current
        && ports.contains(&listener.port)
    {
        release(listener.port).await;
    }
}

/// Closes the challenge-only listener bound to `port`, waiting until its
/// socket is closed. Returns false if there is none.
async fn release(port: u16) -> bool {
    let task = {
        let mut current = CHALLENGE_LISTENER
            .lock()
            .expect("nothing panics while holding the challenge listener lock");
        let Some(listener) = current.upgrade().filter(|listener| listener.port == port) else {
            return false;
        };
        *current = Weak::new();
        listener
            .task
            .lock()
            .expect("nothing panics while holding the challenge task lock")
            .take()
    };
    let Some(task) = task else {
        return false;
    };
    task.abort();
    // Resolves once the task, and with it the socket, is dropped.
    let _ = task.await;
    println!("ACME => Closed the challenge listener on port {port} for the new configuration");
    true
}

fn bind_exclusive(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = if address.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    // Only lets the port be rebound while old connections are in TIME_WAIT.
    #[cfg(not(windows))]
    socket.set_reuseaddr(true)?;
    socket
        .bind(address)
        .map_err(|err| crate::server::listen_error(address, err))?;
    socket.listen(128)
}

async fn serve_challenges(listener: TcpListener) {
    let mut connections = JoinSet::new();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(err) => {
                // Usually out of file descriptors; give it a moment.
                eprintln!("ACME => Challenge listener accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        connections.spawn(async move {
            let service = service_fn(
                |request: hyper::Request<hyper::body::Incoming>| async move {
                    let response = challenge_response(request.method(), request.uri().path())
                        .unwrap_or_else(|| {
                            response::plain(StatusCode::NOT_FOUND, "HTTP 404 NOT FOUND")
                        });
                    Ok::<_, Infallible>(response)
                },
            );
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
        while connections.try_join_next().is_some() {}
    }
}

#[cfg(test)]
mod tests {
    use http_body_util::BodyExt;
    use tokio_rustls::rustls::ServerConfig;

    use super::*;

    fn certificate(not_after: (i32, u8, u8)) -> (String, String) {
        let mut params = rcgen::CertificateParams::new(vec![String::from("a.test")]).unwrap();
        params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        (cert.pem(), key.serialize_pem())
    }

    #[test]
    fn renewal_follows_expiry() {
        let (pem, _) = certificate((2030, 1, 31));
        let expiry = not_after(pem.as_bytes()).unwrap();
        let day = Duration::from_secs(24 * 60 * 60);
        let renew_before = 30 * day;

        assert!(!renewal_due(
            Some(pem.as_bytes()),
            expiry - 31 * day,
            renew_before
        ));
        assert!(renewal_due(
            Some(pem.as_bytes()),
            expiry - 29 * day,
            renew_before
        ));
        assert!(renewal_due(
            Some(pem.as_bytes()),
            expiry + day,
            renew_before
        ));
        assert!(renewal_due(None, expiry - 365 * day, renew_before));
        assert!(renewal_due(
            Some(b"garbage"),
            SystemTime::UNIX_EPOCH,
            renew_before
        ));
        assert_eq!(
            expiry,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_896_048_000),
            "2030-01-31T00:00:00Z"
        );
    }

    #[test]
    fn storage_layout_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let ca = "https://acme-v02.api.letsencrypt.org/directory";
        let storage = Storage::new(dir.path().join("certs"), ca);
        let ca_dir = "certs/acme-v02.api.letsencrypt.org_directory";
        storage.save("a.test", b"CERT", b"KEY").unwrap();
        assert_eq!(
            fs::read(dir.path().join(ca_dir).join("a.test/cert.pem")).unwrap(),
            b"CERT"
        );
        assert_eq!(
            fs::read(dir.path().join(ca_dir).join("a.test/key.pem")).unwrap(),
            b"KEY"
        );
        // Another CA (staging to production) does not see these certificates.
        let staging = Storage::new(
            dir.path().join("certs"),
            "https://acme-staging-v02.api.letsencrypt.org/directory",
        );
        assert!(staging.load("a.test").is_err());
        storage.save("a.test", b"CERT2", b"KEY2").unwrap();
        assert_eq!(
            storage.load("a.test").unwrap(),
            (b"CERT2".to_vec(), b"KEY2".to_vec())
        );
        assert!(storage.load("b.test").is_err());
        let leftovers = fs::read_dir(dir.path().join(ca_dir).join("a.test"))
            .unwrap()
            .count();
        assert_eq!(leftovers, 2, "no temporary files are left behind");
        assert!(storage.save("../escape", b"C", b"K").is_err());
        assert!(storage.save("*.../escape", b"C", b"K").is_err());
        storage.save("*.a.test", b"WILD", b"KEY").unwrap();
        assert_eq!(
            fs::read(dir.path().join(ca_dir).join("wildcard_.a.test/cert.pem")).unwrap(),
            b"WILD"
        );
        assert_eq!(storage.load("*.a.test").unwrap().0, b"WILD");
        assert!(storage.load("..").is_err());

        assert_eq!(
            storage.account_path(),
            dir.path()
                .join("certs/accounts/acme-v02.api.letsencrypt.org_directory.json")
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &str| {
                fs::metadata(dir.path().join(path))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            };
            let ca_dir = "certs/acme-v02.api.letsencrypt.org_directory";
            assert_eq!(mode(&format!("{ca_dir}/a.test/key.pem")), 0o600);
            assert_eq!(mode(&format!("{ca_dir}/a.test")), 0o700);
            assert_eq!(mode(ca_dir), 0o700);
        }
    }

    #[test]
    fn stored_certificates_reach_the_resolver() {
        let resolver = Arc::new(CertResolver::new(Arc::clone(
            ServerConfig::builder().crypto_provider(),
        )));
        resolver.reserve(&[String::from("a.test")]);
        let domain = Domain {
            name: String::from("a.test"),
            resolvers: vec![Arc::clone(&resolver)],
        };
        let (cert, key) = certificate((2030, 1, 1));
        assert!(install(&domain, cert.as_bytes(), b"not a key").is_err());
        assert!(resolver.select(Some("a.test")).is_none());
        install(&domain, cert.as_bytes(), key.as_bytes()).unwrap();
        assert!(resolver.select(Some("a.test")).is_some());
    }

    #[test]
    fn issuance_skips_names_in_flight_or_backing_off() {
        const CA: &str = "https://ca.test/dir";
        let mut issuance = Issuance::default();
        let start = Instant::now();
        assert_eq!(issuance.claim("a.test", CA, start), Claim::Granted);
        assert_eq!(issuance.claim("a.test", CA, start), Claim::InFlight);
        assert_eq!(
            issuance.claim("a.test", "https://other.test/dir", start),
            Claim::InFlight,
            "one order per name, whatever the CA"
        );
        assert_eq!(issuance.claim("b.test", CA, start), Claim::Granted);

        issuance.finish("a.test", CA, Outcome::Failed, start);
        let later = start + Duration::from_secs(600);
        assert_eq!(
            issuance.claim("a.test", CA, later),
            Claim::BackingOff(RETRY_INTERVAL - Duration::from_secs(600))
        );
        assert_eq!(
            issuance.claim("a.test", CA, start + RETRY_INTERVAL),
            Claim::Granted
        );
        // A configuration problem neither starts nor clears the backoff.
        issuance.finish("a.test", CA, Outcome::NotAttempted, start + RETRY_INTERVAL);
        assert_eq!(
            issuance.claim("a.test", CA, start + RETRY_INTERVAL),
            Claim::Granted
        );
        issuance.finish("a.test", CA, Outcome::Failed, start);
        issuance.claim("a.test", CA, start + RETRY_INTERVAL);
        issuance.finish("a.test", CA, Outcome::NotAttempted, later);
        assert!(matches!(
            issuance.claim("a.test", CA, later),
            Claim::BackingOff(_)
        ));
        // The backoff belongs to the CA that failed.
        assert_eq!(
            issuance.claim("a.test", "https://other.test/dir", later),
            Claim::Granted
        );

        issuance.finish("b.test", CA, Outcome::NotAttempted, start);
        assert_eq!(issuance.claim("b.test", CA, start), Claim::Granted);
        issuance.finish("b.test", CA, Outcome::Succeeded, start);
        assert_eq!(issuance.claim("b.test", CA, start), Claim::Granted);
    }

    #[test]
    fn challenge_port_needs_a_plain_http_server_or_a_free_port() {
        let port = |servers: &str| {
            let text = format!("[acme]\nhttp_port = 8080\n{servers}");
            ChallengePort::of(&Config::from_toml(&text).unwrap())
        };
        let server = |listen: &str, extra: &str| {
            format!("[[server]]\nlisten = \"{listen}\"\nforward = \"a:1\"\n{extra}\n")
        };
        let tls = "tls = { cert = \"c.pem\", key = \"k.pem\" }";

        assert_eq!(
            port(&server("0.0.0.0:443", tls)),
            ChallengePort::Listener("0.0.0.0:8080".parse().unwrap())
        );
        assert_eq!(
            port(&server("0.0.0.0:8080", "protocol = \"udp\"")),
            ChallengePort::Listener("0.0.0.0:8080".parse().unwrap())
        );
        assert_eq!(port(&server("0.0.0.0:8080", "")), ChallengePort::Served);
        assert!(matches!(
            port(&server("0.0.0.0:8080", tls)),
            ChallengePort::Blocked(why) if why.contains("with TLS")
        ));
        assert!(matches!(
            port(&server("0.0.0.0:8080", "protocol = \"tcp\"")),
            ChallengePort::Blocked(why) if why.contains("server[0] (tcp)")
        ));
        // TLS-ALPN-01 does not use the port at all.
        assert_eq!(
            port(&format!(
                "challenge = \"tls-alpn-01\"\n{}",
                server("0.0.0.0:8080", "protocol = \"tcp\"")
            )),
            ChallengePort::Served
        );
        assert_eq!(
            port(&format!(
                "challenge = \"dns-01\"\n{}[acme.dns]\nprovider = \"cloudflare\"\n\
                 api_token = \"t\"\n",
                server("0.0.0.0:8080", "protocol = \"tcp\"")
            )),
            ChallengePort::Served
        );
        assert_eq!(
            port(&format!(
                "{}{}",
                server("10.0.0.1:8080", "protocol = \"tcp\""),
                server("10.0.0.2:8080", "")
            )),
            ChallengePort::Served
        );
    }

    // One test, because the listener is process wide.
    #[tokio::test]
    async fn challenge_listener_is_shared_exclusive_and_released_on_demand() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let err = ChallengeListener::acquire(taken.local_addr().unwrap())
            .err()
            .expect("the port is taken");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);

        let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let first = ChallengeListener::acquire(address).unwrap();
        let second = ChallengeListener::acquire_soon(address).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second), "rounds share one listener");
        drop(second);

        // A new configuration takes the port while the round still holds
        // the listener.
        let port = first.port;
        assert!(!release(port.wrapping_add(1)).await, "another port");
        let config = Config::from_toml(&format!(
            "[[server]]\nlisten = \"127.0.0.1:{port}\"\nforward = \"a:1\"\n"
        ))
        .unwrap();
        release_challenge_port(&config).await;
        let rebound = std::net::TcpListener::bind(("127.0.0.1", port));
        assert!(rebound.is_ok(), "the port is free right after the release");
        drop(rebound);

        let third = ChallengeListener::acquire(address).unwrap();
        assert!(
            !Arc::ptr_eq(&first, &third),
            "a released listener is not reused"
        );
        // The round that held the released listener ends without trouble.
        drop(first);
        assert!(release(third.port).await);
    }

    #[tokio::test]
    async fn challenge_responder_answers_pending_tokens_only() {
        let path = "/.well-known/acme-challenge/token-123";
        assert!(challenge_response(&Method::GET, path).is_none());

        let mut published = Published::default();
        published.add(String::from("token-123"), String::from("token-123.thumb"));
        assert!(challenge_response(&Method::POST, path).is_none());
        assert!(challenge_response(&Method::GET, "/token-123").is_none());
        let response = challenge_response(&Method::GET, path).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, "token-123.thumb");

        drop(published);
        assert!(challenge_response(&Method::GET, path).is_none());
    }

    #[tokio::test]
    async fn dns_records_are_deleted_when_the_order_ends_or_is_cancelled() {
        let server = rxh_test_util::DnsServer::spawn("127.0.0.1:0", "a.test").await;
        let config = Config::from_toml(&format!(
            "[[server]]\nlisten = \"127.0.0.1:443\"\nforward = \"a:1\"\n\
             [acme.dns]\nprovider = \"rfc2136\"\nserver = \"{}\"\n",
            server.address()
        ))
        .unwrap();
        let dns = config.acme.dns.unwrap();
        let name = "_acme-challenge.a.test";
        let publish = async |value: &str| {
            let record = dns01::publish(&dns, name, value).await.unwrap();
            let mut published = Published::default();
            published.records.push((dns.clone(), record));
            published
        };

        let mut published = publish("ended").await;
        assert_eq!(server.txt(name), ["ended"]);
        published.withdraw_records().await;
        assert!(server.txt(name).is_empty());

        // A timed out order only drops what it published.
        drop(publish("cancelled").await);
        for _ in 0..200 {
            if server.txt(name).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(server.txt(name).is_empty(), "deleted in the background");
    }

    /// TLS handshake with `address` for `name`, offering only `alpn`.
    /// Returns the leaf certificate, the negotiated protocol and whether the
    /// server closed the connection right after.
    async fn handshake(
        address: SocketAddr,
        name: &str,
        alpn: &[u8],
    ) -> io::Result<(Vec<u8>, Option<Vec<u8>>, bool)> {
        use tokio::io::AsyncReadExt;
        use tokio_rustls::rustls::ClientConfig;
        use tokio_rustls::rustls::pki_types::ServerName;

        let mut client = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate))
            .with_no_client_auth();
        client.alpn_protocols = vec![alpn.to_vec()];
        let stream = tokio::net::TcpStream::connect(address).await?;
        let server_name = ServerName::try_from(name.to_owned()).unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(client))
            .connect(server_name, stream)
            .await?;
        let connection = tls.get_ref().1;
        let leaf = connection.peer_certificates().unwrap()[0].to_vec();
        let negotiated = connection.alpn_protocol().map(<[u8]>::to_vec);
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(2), tls.read(&mut byte)).await;
        // A close without close_notify is an error, and still a close.
        let closed = matches!(read, Ok(Ok(0) | Err(_)));
        Ok((leaf, negotiated, closed))
    }

    #[tokio::test]
    async fn tls_alpn_challenge_certificate_is_served_to_validations_only() {
        let config = Config::from_toml(
            "[[server]]\nlisten = \"127.0.0.1:0\"\nhost = \"alpn.test\"\ntls = \"auto\"\n\
             forward = \"127.0.0.1:1\"\n",
        )
        .unwrap();
        let master = Master::from_config(&config).unwrap();
        let address = master.sockets()[0];
        let domain = Domain {
            name: String::from("alpn.test"),
            resolvers: master.cert_resolvers(0),
        };
        tokio::task::spawn(master.run());

        // No pending challenge: a validation handshake fails.
        assert!(
            handshake(address, "alpn.test", TLS_ALPN_PROTOCOL)
                .await
                .is_err()
        );

        let digest = [7; 32];
        let mut published = Published::default();
        published.add_certificate(
            &domain.name,
            challenge_certified_key(&domain, &digest).unwrap(),
        );

        // Case insensitive, as SNI is.
        let (leaf, negotiated, closed) = handshake(address, "ALPN.test", TLS_ALPN_PROTOCOL)
            .await
            .unwrap();
        assert_eq!(negotiated.as_deref(), Some(TLS_ALPN_PROTOCOL));
        assert!(closed, "nothing is served after a validation handshake");
        let (_, certificate) = x509_parser::parse_x509_certificate(&leaf).unwrap();
        let names = certificate.subject_alternative_name().unwrap().unwrap();
        assert_eq!(
            names.value.general_names,
            [x509_parser::extensions::GeneralName::DNSName("alpn.test")]
        );
        let identifier = certificate
            .extensions()
            .iter()
            .find(|extension| extension.oid.to_id_string() == "1.3.6.1.5.5.7.1.31")
            .expect("the acmeIdentifier extension");
        assert!(identifier.critical);
        // DER octet string of the 32 digest bytes.
        assert_eq!(identifier.value, [&[0x04, 0x20][..], &digest[..]].concat());

        // Only for that name, and never to ordinary clients.
        assert!(
            handshake(address, "other.test", TLS_ALPN_PROTOCOL)
                .await
                .is_err()
        );
        assert!(handshake(address, "alpn.test", b"http/1.1").await.is_err());

        // The real certificate goes to ordinary clients only.
        let (cert, key) = certificate_for("alpn.test");
        install(&domain, cert.as_bytes(), key.as_bytes()).unwrap();
        let (real, negotiated, closed) =
            handshake(address, "alpn.test", b"http/1.1").await.unwrap();
        assert_ne!(real, leaf);
        assert_eq!(negotiated.as_deref(), Some(&b"http/1.1"[..]));
        assert!(!closed, "an ordinary connection waits for a request");
        let (again, ..) = handshake(address, "alpn.test", TLS_ALPN_PROTOCOL)
            .await
            .unwrap();
        assert_eq!(again, leaf);

        drop(published);
        assert!(
            handshake(address, "alpn.test", TLS_ALPN_PROTOCOL)
                .await
                .is_err(),
            "a withdrawn challenge, even with a real certificate installed"
        );
    }

    /// Accepts any server certificate; the test inspects it itself.
    #[derive(Debug)]
    struct AnyCertificate;

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for AnyCertificate {
        fn verify_server_cert(
            &self,
            _: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
            _: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: tokio_rustls::rustls::pki_types::UnixTime,
        ) -> Result<
            tokio_rustls::rustls::client::danger::ServerCertVerified,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
            ServerConfig::builder()
                .crypto_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    fn certificate_for(name: &str) -> (String, String) {
        let certified = rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap();
        (certified.cert.pem(), certified.signing_key.serialize_pem())
    }
}
