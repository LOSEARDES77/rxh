//! Sessions of the users signed in to the admin API, the failed sign-ins that
//! lock a name out, and the setup code of the first run. All of it lives in
//! memory: a restart signs everybody out and makes a new code.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hyper::Request;
use hyper::header::{self, HeaderValue};

/// Name of the session cookie.
const COOKIE: &str = "rxh_session";

/// Time a session lasts from sign-in. Using it does not extend it.
const LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// Sessions kept at most; the oldest goes when one more is created.
const MAX_SESSIONS: usize = 1000;

/// Sessions one user keeps at most; its own oldest goes when it starts one
/// more, so that signing in again and again ends nobody else's.
const MAX_SESSIONS_PER_USER: usize = 10;

/// Entries the tables of failed sign-ins and of attempts hold before the
/// stale ones are looked for. Below it nothing is scanned.
const PRUNE_ABOVE: usize = 1024;

/// Failed sign-ins for a name from one address that lock the pair out.
const MAX_FAILURES: u32 = 5;

/// Time a name stays locked for an address after its last failure, and time
/// after which older failures no longer count.
const LOCKOUT: Duration = Duration::from_secs(60);

/// Sign-ins, right or wrong and under any name, one client address may try
/// within [`LOCKOUT`]. Each can cost a hash, and the per name count does
/// not stop a client that changes the name every time.
const MAX_ATTEMPTS_PER_ADDRESS: usize = 10;

/// Characters of the setup code: no `i`, `l`, `0` or `1`, which read alike.
/// Their number divides 256, so a random byte picks one without bias.
const CODE_ALPHABET: &[u8; 32] = b"abcdefghjkmnopqrstuvwxyz23456789";

/// Length of the setup code.
const CODE_LENGTH: usize = 10;

/// A signed-in user.
#[derive(Clone)]
pub(crate) struct Session {
    /// Name of the user.
    pub(crate) user: String,
    /// When the user signed in.
    pub(crate) created: Instant,
    /// Roles fixed at sign-in, for a user that is not in the users file
    /// (OIDC). `None` for a local user, whose roles are read from the file
    /// on every request.
    pub(crate) roles: Option<Vec<String>>,
    /// bcrypt hash of the password the user had in the users file at
    /// sign-in. The session ends once the file holds another one, whoever
    /// changed it. `None` for a session that carries its roles.
    pub(crate) password: Option<String>,
}

/// Failed sign-ins of one name from one address.
struct Failures {
    count: u32,
    last: Instant,
}

struct Tables {
    /// Sessions by id.
    sessions: HashMap<String, Session>,
    /// Failed sign-ins by name and client (`None` on a unix socket), see
    /// [`client`].
    failures: HashMap<(String, Option<IpAddr>), Failures>,
    /// Size of `failures` from which its stale entries are removed.
    prune_failures_at: usize,
    /// When each client tried to sign in, within the last minute.
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
    /// Size of `attempts` from which its stale entries are removed.
    prune_attempts_at: usize,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            failures: HashMap::new(),
            prune_failures_at: PRUNE_ABOVE,
            attempts: HashMap::new(),
            prune_attempts_at: PRUNE_ABOVE,
        }
    }
}

/// What the limits count as one client: an IPv4 address, also when it comes
/// mapped in an IPv6 one, or the /64 of an IPv6 address, since one host
/// usually has all of its /64 to send from.
fn client(address: Option<IpAddr>) -> Option<IpAddr> {
    address.map(|address| match address.to_canonical() {
        IpAddr::V6(address) => {
            let network = u128::from(address) & !u128::from(u64::MAX);
            IpAddr::V6(Ipv6Addr::from(network))
        }
        v4 @ IpAddr::V4(_) => v4,
    })
}

/// The size a table is next pruned at, once it holds `len` live entries:
/// twice that, so the scans cost little per entry however many clients try.
fn next_prune(len: usize) -> usize {
    PRUNE_ABOVE.max(len * 2)
}

/// The sessions, the failed sign-ins and the setup code of one daemon.
pub(crate) struct Sessions {
    tables: Mutex<Tables>,
    setup_code: String,
}

impl Sessions {
    pub(crate) fn new() -> Self {
        let mut code = [0_u8; CODE_LENGTH];
        fill_random(&mut code);
        let setup_code: String = code
            .iter()
            .map(|byte| char::from(CODE_ALPHABET[usize::from(*byte) % CODE_ALPHABET.len()]))
            .collect();
        Self {
            tables: Mutex::new(Tables::default()),
            setup_code,
        }
    }

    /// Starts a session of `user`, whose password hash is `password`, and
    /// returns its id: 32 random bytes in base64url.
    pub(crate) fn create(
        &self,
        user: &str,
        roles: Option<Vec<String>>,
        password: Option<&str>,
    ) -> String {
        self.create_at(user, roles, password, Instant::now())
    }

    /// The session with this id, until it is 12 hours old.
    pub(crate) fn get(&self, id: &str) -> Option<Session> {
        self.get_at(id, Instant::now())
    }

    /// Ends one session.
    pub(crate) fn end(&self, id: &str) {
        self.lock().sessions.remove(id);
    }

    /// Ends every session of `user`.
    pub(crate) fn end_user(&self, user: &str) {
        self.lock()
            .sessions
            .retain(|_, session| session.user != user);
    }

    /// Counts one sign-in as `name` from `address`, unless it is refused,
    /// which returns false: the address tried 10 times in the last minute,
    /// whatever the names, or the pair is locked out (5 failures, the last
    /// one less than a minute ago). The sign-in counts as failed until a
    /// good one takes that back with [`Sessions::forget`]. One step, so
    /// that attempts made at the same time cannot all pass the check before
    /// any of them is counted.
    pub(crate) fn attempt(&self, name: &str, address: Option<IpAddr>) -> bool {
        self.attempt_at(name, address, Instant::now())
    }

    /// Counts one guess of the setup code from `address` among its
    /// sign-ins. False when the address has tried 10 times in the last
    /// minute.
    pub(crate) fn guess(&self, address: Option<IpAddr>) -> bool {
        self.guess_at(address, Instant::now())
    }

    /// Forgets the failed sign-ins as `name` from `address`, after a good one.
    pub(crate) fn forget(&self, name: &str, address: Option<IpAddr>) {
        let key = (name.to_owned(), client(address));
        self.lock().failures.remove(&key);
    }

    /// Takes back the failure that [`Sessions::attempt`] counted for a
    /// sign-in whose password was never compared. The attempt of the address
    /// stays counted.
    pub(crate) fn uncount(&self, name: &str, address: Option<IpAddr>) {
        let key = (name.to_owned(), client(address));
        if let Some(failures) = self.lock().failures.get_mut(&key) {
            failures.count = failures.count.saturating_sub(1);
        }
    }

    /// The code that lets a remote caller create the first user: 10
    /// characters, new in every process.
    pub(crate) fn setup_code(&self) -> &str {
        &self.setup_code
    }

    fn create_at(
        &self,
        user: &str,
        roles: Option<Vec<String>>,
        password: Option<&str>,
        now: Instant,
    ) -> String {
        use base64::Engine;

        let mut bytes = [0_u8; 32];
        fill_random(&mut bytes);
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut tables = self.lock();
        tables.sessions.retain(|_, session| !expired(session, now));
        let own = tables
            .sessions
            .values()
            .filter(|session| session.user == user)
            .count();
        let full = tables.sessions.len() >= MAX_SESSIONS;
        if own >= MAX_SESSIONS_PER_USER || full {
            let oldest = tables
                .sessions
                .iter()
                .filter(|(_, session)| own < MAX_SESSIONS_PER_USER || session.user == user)
                .min_by_key(|(_, session)| session.created)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                tables.sessions.remove(&oldest);
            }
        }
        let session = Session {
            user: user.to_owned(),
            created: now,
            roles,
            password: password.map(str::to_owned),
        };
        tables.sessions.insert(id.clone(), session);
        id
    }

    fn get_at(&self, id: &str, now: Instant) -> Option<Session> {
        let mut tables = self.lock();
        if expired(tables.sessions.get(id)?, now) {
            tables.sessions.remove(id);
            return None;
        }
        tables.sessions.get(id).cloned()
    }

    fn guess_at(&self, address: Option<IpAddr>, now: Instant) -> bool {
        address_may_try(&mut self.lock(), client(address), now)
    }

    fn attempt_at(&self, name: &str, address: Option<IpAddr>, now: Instant) -> bool {
        let address = client(address);
        let mut tables = self.lock();
        if !address_may_try(&mut tables, address, now) {
            return false;
        }
        let is_stale =
            |failures: &Failures| now.saturating_duration_since(failures.last) >= LOCKOUT;
        // Names nobody retries leave once the table has grown, not on every
        // attempt: all of them wait on this lock.
        if tables.failures.len() >= tables.prune_failures_at {
            tables.failures.retain(|_, failures| !is_stale(failures));
            tables.prune_failures_at = next_prune(tables.failures.len());
        }
        let failures = tables
            .failures
            .entry((name.to_owned(), address))
            .or_insert(Failures {
                count: 0,
                last: now,
            });
        if is_stale(failures) {
            failures.count = 0;
        }
        if failures.count >= MAX_FAILURES {
            return false;
        }
        failures.count += 1;
        failures.last = now;
        true
    }

    fn lock(&self) -> MutexGuard<'_, Tables> {
        self.tables
            .lock()
            .expect("no code panics while holding the sessions lock")
    }
}

/// Counts one attempt from the client `address` (see [`client`]), unless it
/// made [`MAX_ATTEMPTS_PER_ADDRESS`] in the last minute, which returns
/// false. A connection without an address (the unix socket) is not limited:
/// only the owner of the daemon reaches it.
fn address_may_try(tables: &mut Tables, address: Option<IpAddr>, now: Instant) -> bool {
    let Some(address) = address else {
        return true;
    };
    let is_recent = |at: &Instant| now.saturating_duration_since(*at) < LOCKOUT;
    // Clients that stopped trying leave once the table has grown.
    if tables.attempts.len() >= tables.prune_attempts_at {
        tables
            .attempts
            .retain(|_, times| times.back().is_some_and(is_recent));
        tables.prune_attempts_at = next_prune(tables.attempts.len());
    }
    let times = tables.attempts.entry(address).or_default();
    while times.front().is_some_and(|at| !is_recent(at)) {
        times.pop_front();
    }
    if times.len() >= MAX_ATTEMPTS_PER_ADDRESS {
        return false;
    }
    times.push_back(now);
    true
}

fn expired(session: &Session, now: Instant) -> bool {
    now.saturating_duration_since(session.created) >= LIFETIME
}

fn fill_random(bytes: &mut [u8]) {
    aws_lc_rs::rand::fill(bytes).expect("the random generator of the system works");
}

/// The `Set-Cookie` value that gives a browser the session `id`. `secure`
/// on a TLS connection: the browser then sends it over HTTPS only.
pub(crate) fn cookie(id: &str, secure: bool) -> HeaderValue {
    set_cookie(id, secure, "")
}

/// The `Set-Cookie` value that makes a browser drop the session cookie.
pub(crate) fn expired_cookie(secure: bool) -> HeaderValue {
    set_cookie("", secure, "; Max-Age=0")
}

fn set_cookie(id: &str, secure: bool, rest: &str) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    let value = format!("{COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/{secure}{rest}");
    HeaderValue::from_str(&value).expect("a session id is base64url, which a header can hold")
}

/// The session id a request carries in its cookie.
pub(crate) fn cookie_of<B>(request: &Request<B>) -> Option<&str> {
    for value in request.headers().get_all(header::COOKIE) {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            let id = pair
                .trim()
                .strip_prefix(COOKIE)
                .and_then(|rest| rest.strip_prefix('='));
            if id.is_some() {
                return id;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::net::Ipv4Addr;

    use super::*;

    #[test]
    fn a_session_ends_after_twelve_hours() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let id = sessions.create_at("ana", None, None, start);
        let almost = start + LIFETIME - Duration::from_secs(1);
        let session = sessions.get_at(&id, almost).expect("still valid");
        assert_eq!(session.user, "ana");
        assert_eq!(session.roles, None);
        // Reading it a second before the end did not extend it.
        assert!(sessions.get_at(&id, start + LIFETIME).is_none());
        assert!(
            sessions.get_at(&id, start).is_none(),
            "an ended session is gone"
        );
        assert!(sessions.get_at("no such id", start).is_none());
    }

    #[test]
    fn the_oldest_session_goes_when_there_are_a_thousand() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let ids: Vec<String> = (0..1000_u32)
            .map(|index| {
                let user = format!("user{index}");
                sessions.create_at(&user, None, None, start + second * index)
            })
            .collect();
        let now = start + second * 1000;
        assert!(sessions.get_at(&ids[0], now).is_some(), "a thousand fit");

        let newest = sessions.create_at("luis", None, None, now);
        assert!(sessions.get_at(&ids[0], now).is_none(), "the oldest went");
        assert!(sessions.get_at(&ids[1], now).is_some());
        assert!(sessions.get_at(&newest, now).is_some());
        assert_eq!(sessions.lock().sessions.len(), 1000);

        // Sessions that ended make room first.
        let later = start + LIFETIME + second * 500;
        sessions.create_at("eva", None, None, later);
        assert_eq!(sessions.lock().sessions.len(), 501);
    }

    #[test]
    fn a_user_that_signs_in_again_and_again_ends_only_its_own_sessions() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let others: Vec<String> = (0..990_u32)
            .map(|index| sessions.create_at(&format!("user{index}"), None, None, start))
            .collect();
        let own: Vec<String> = (1..=10_u32)
            .map(|index| sessions.create_at("eva", None, None, start + second * index))
            .collect();
        let now = start + second * 11;
        assert!(sessions.get_at(&own[0], now).is_some(), "ten fit");

        // The table is full and `eva` signs in a thousand times more.
        let mut newest = String::new();
        for _ in 0..1000 {
            newest = sessions.create_at("eva", None, None, now);
        }
        for id in &others {
            assert!(sessions.get_at(id, now).is_some(), "of another user");
        }
        assert!(sessions.get_at(&own[0], now).is_none(), "its own oldest");
        assert!(sessions.get_at(&newest, now).is_some());
        assert_eq!(sessions.lock().sessions.len(), 1000);

        // A user below ten on a full table takes the place of the oldest.
        let luis = sessions.create_at("luis", None, None, now);
        assert!(sessions.get_at(&luis, now).is_some());
        assert_eq!(sessions.lock().sessions.len(), 1000);
        let gone = others
            .iter()
            .filter(|id| sessions.get_at(id, now).is_none());
        assert_eq!(gone.count(), 1);
    }

    #[test]
    fn ending_a_user_ends_all_its_sessions_and_no_other() {
        let sessions = Sessions::new();
        let (first, second) = (
            sessions.create("ana", None, None),
            sessions.create("ana", None, None),
        );
        let other = sessions.create("luis", Some(vec![String::from("web")]), None);
        sessions.end_user("ana");
        assert!(sessions.get(&first).is_none());
        assert!(sessions.get(&second).is_none());
        let kept = sessions.get(&other).expect("another user");
        assert_eq!(kept.roles, Some(vec![String::from("web")]));
        sessions.end(&other);
        assert!(sessions.get(&other).is_none());
    }

    #[test]
    fn lockout_is_per_name_and_address_and_lasts_a_minute() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let here = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
        let there = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)));
        for _ in 0..5 {
            assert!(sessions.attempt_at("ana", here, start), "five are allowed");
        }
        assert!(!sessions.attempt_at("ana", here, start));
        assert!(sessions.attempt_at("luis", here, start), "another name");
        assert!(sessions.attempt_at("ana", there, start), "another client");
        assert!(sessions.attempt_at("ana", None, start), "the unix socket");

        // A refused attempt does not make the minute longer.
        let minute = Duration::from_secs(60);
        assert!(!sessions.attempt_at("ana", here, start + minute - Duration::from_secs(1)));
        assert!(sessions.attempt_at("ana", here, start + minute));
        // The count started again.
        for _ in 0..4 {
            assert!(sessions.attempt_at("ana", here, start + minute));
        }
        assert!(!sessions.attempt_at("ana", here, start + minute));

        // A good sign-in takes the failures back.
        sessions.forget("ana", here);
        assert!(sessions.attempt_at("ana", here, start + minute));
    }

    #[test]
    fn an_address_tries_ten_times_a_minute_whatever_the_name() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let here = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
        let there = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 8)));
        // A name each time, one attempt a second, one of them a setup code.
        for index in 0..9_u32 {
            let name = format!("user{index}");
            assert!(sessions.attempt_at(&name, here, start + second * index));
        }
        assert!(sessions.guess_at(here, start + second * 9));
        let now = start + second * 10;
        assert!(!sessions.attempt_at("ana", here, now), "the eleventh");
        assert!(!sessions.guess_at(here, now));
        assert!(sessions.attempt_at("ana", there, now), "another client");
        assert!(sessions.guess_at(there, now));
        // A good sign-in takes back the failure of its name, not the attempt.
        sessions.forget("user0", here);
        assert!(!sessions.attempt_at("user0", here, now));

        // Each attempt leaves the count a minute after it was made, and the
        // refused ones were never in it.
        let minute = Duration::from_secs(60);
        assert!(!sessions.attempt_at("ana", here, start + minute - second));
        assert!(sessions.attempt_at("ana", here, start + minute));
        assert!(!sessions.attempt_at("ana", here, start + minute));
        assert!(sessions.attempt_at("ana", here, start + minute + second));

        // The unix socket has no address to count.
        for index in 0..30 {
            assert!(sessions.attempt_at(&format!("user{index}"), None, start));
            assert!(sessions.guess_at(None, start));
        }
    }

    #[test]
    fn the_addresses_of_one_ipv6_network_are_one_client() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let host = |text: &str| Some(text.parse::<IpAddr>().unwrap());
        // Another address of the same /64 each time.
        for index in 0..5_u32 {
            let address = host(&format!("2001:db8:0:1::{index:x}"));
            assert!(
                sessions.attempt_at("ana", address, start),
                "five are allowed"
            );
        }
        assert!(!sessions.attempt_at("ana", host("2001:db8:0:1:ffff::9"), start));
        assert!(
            sessions.attempt_at("ana", host("2001:db8:0:2::1"), start),
            "another /64"
        );
        // Ten attempts of the network, whatever the name and the address:
        // six were made, the refused one too.
        for index in 0..3_u32 {
            let address = host(&format!("2001:db8:0:1:{index:x}::1"));
            assert!(sessions.attempt_at(&format!("user{index}"), address, start));
        }
        assert!(
            sessions.guess_at(host("2001:db8:0:1::a"), start),
            "the tenth"
        );
        assert!(!sessions.attempt_at("luis", host("2001:db8:0:1::b"), start));
        assert!(!sessions.guess_at(host("2001:db8:0:1::c"), start));

        // A good sign-in from one address forgets what the others failed.
        sessions.forget("ana", host("2001:db8:0:1::77"));
        let minute = Duration::from_secs(60);
        for _ in 0..5 {
            assert!(sessions.attempt_at("ana", host("2001:db8:0:1::1"), start + minute));
        }

        // An IPv4 address is the same client when it comes mapped.
        for _ in 0..5 {
            assert!(sessions.attempt_at("ana", host("::ffff:192.0.2.7"), start));
        }
        assert!(!sessions.attempt_at("ana", host("192.0.2.7"), start));
        assert!(
            sessions.attempt_at("ana", host("192.0.2.8"), start),
            "not a /64"
        );
    }

    #[test]
    fn an_attempt_taken_back_is_not_a_failure_of_the_name() {
        let sessions = Sessions::new();
        let start = Instant::now();
        let here = Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7)));
        // Four sign-ins that never got to compare a password.
        for _ in 0..4 {
            assert!(sessions.attempt_at("ana", here, start));
            sessions.uncount("ana", here);
        }
        // Still a failure each when not taken back, and five lock the name.
        for _ in 0..5 {
            assert!(sessions.attempt_at("ana", here, start));
        }
        assert!(!sessions.attempt_at("ana", here, start));
        // Taking one back for a name that was never counted does nothing.
        sessions.uncount("luis", here);
        assert!(sessions.attempt_at("luis", here, start + LOCKOUT * 2));
    }

    #[test]
    fn stale_entries_leave_when_a_table_grows_and_not_on_every_attempt() {
        let start = Instant::now();
        let later = start + LOCKOUT;
        let address = |index: u32| Some(IpAddr::V4(Ipv4Addr::from(0xc000_0200 + index)));
        let filled = |entries: u32| {
            let sessions = Sessions::new();
            for index in 0..entries {
                assert!(sessions.attempt_at(&format!("user{index}"), address(index), start));
            }
            sessions
        };

        // A minute later they no longer count, and nothing looked for them.
        let small = filled(100);
        assert!(small.attempt_at("ana", address(5000), later));
        assert_eq!(small.lock().failures.len(), 101);
        assert_eq!(small.lock().attempts.len(), 101);

        // Once the tables are large the stale entries go, all at once.
        let large = filled(1024);
        assert!(large.attempt_at("ana", address(5000), later));
        assert_eq!(large.lock().failures.len(), 1);
        assert_eq!(large.lock().attempts.len(), 1);
        // And while they are live they stay, however many they are.
        let live = filled(1500);
        assert!(live.attempt_at("ana", address(5000), start));
        assert_eq!(live.lock().failures.len(), 1501);
    }

    #[test]
    fn attempts_at_the_same_time_are_all_counted() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;

        let sessions = &Sessions::new();
        let allowed = &AtomicUsize::new(0);
        thread::scope(|scope| {
            for _ in 0..64 {
                scope.spawn(move || {
                    if sessions.attempt("ana", None) {
                        allowed.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(allowed.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn ids_are_43_base64url_characters_and_differ() {
        let sessions = Sessions::new();
        let ids: HashSet<String> = (0..100)
            .map(|_| sessions.create("ana", None, None))
            .collect();
        assert_eq!(ids.len(), 100);
        for id in &ids {
            assert_eq!(id.len(), 43, "{id}");
            assert!(
                id.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "{id}"
            );
        }
    }

    #[test]
    fn the_setup_code_is_ten_characters_and_new_in_every_process() {
        let (first, second) = (Sessions::new(), Sessions::new());
        assert_eq!(first.setup_code().len(), 10);
        assert!(
            first
                .setup_code()
                .bytes()
                .all(|byte| CODE_ALPHABET.contains(&byte))
        );
        assert_ne!(first.setup_code(), second.setup_code());
    }

    #[test]
    fn the_cookie_is_private_and_secure_over_tls() {
        assert_eq!(
            cookie("abc", /* secure= */ false),
            "rxh_session=abc; HttpOnly; SameSite=Strict; Path=/"
        );
        assert_eq!(
            cookie("abc", /* secure= */ true),
            "rxh_session=abc; HttpOnly; SameSite=Strict; Path=/; Secure"
        );
        assert_eq!(
            expired_cookie(/* secure= */ true),
            "rxh_session=; HttpOnly; SameSite=Strict; Path=/; Secure; Max-Age=0"
        );
    }

    #[test]
    fn the_session_id_is_read_from_the_cookie_header() {
        let with = |cookies: &[&str]| {
            let mut builder = Request::builder();
            for cookie in cookies {
                builder = builder.header(header::COOKIE, *cookie);
            }
            builder.body(()).unwrap()
        };
        assert_eq!(cookie_of(&with(&["rxh_session=abc"])), Some("abc"));
        assert_eq!(
            cookie_of(&with(&["theme=dark; rxh_session=abc; x=1"])),
            Some("abc")
        );
        assert_eq!(
            cookie_of(&with(&["theme=dark", "rxh_session=abc"])),
            Some("abc")
        );
        assert_eq!(cookie_of(&with(&["rxh_session_old=abc; theme=dark"])), None);
        assert_eq!(cookie_of(&with(&["not_rxh_session=abc"])), None);
        assert_eq!(cookie_of(&with(&[])), None);
    }
}
