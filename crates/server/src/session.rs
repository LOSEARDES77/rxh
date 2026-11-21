//! Sessions of the users signed in to the admin API, the failed sign-ins that
//! lock a name out, and the setup code of the first run. All of it lives in
//! memory: a restart signs everybody out and makes a new code.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
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
}

/// Failed sign-ins of one name from one address.
struct Failures {
    count: u32,
    last: Instant,
}

#[derive(Default)]
struct Tables {
    /// Sessions by id.
    sessions: HashMap<String, Session>,
    /// Failed sign-ins by name and client address (`None` on a unix socket).
    failures: HashMap<(String, Option<IpAddr>), Failures>,
    /// When each client address tried to sign in, within the last minute.
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
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

    /// Starts a session of `user` and returns its id: 32 random bytes in
    /// base64url.
    pub(crate) fn create(&self, user: &str, roles: Option<Vec<String>>) -> String {
        self.create_at(user, roles, Instant::now())
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
        self.lock().failures.remove(&(name.to_owned(), address));
    }

    /// The code that lets a remote caller create the first user: 10
    /// characters, new in every process.
    pub(crate) fn setup_code(&self) -> &str {
        &self.setup_code
    }

    fn create_at(&self, user: &str, roles: Option<Vec<String>>, now: Instant) -> String {
        use base64::Engine;

        let mut bytes = [0_u8; 32];
        fill_random(&mut bytes);
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut tables = self.lock();
        tables.sessions.retain(|_, session| !expired(session, now));
        if tables.sessions.len() >= MAX_SESSIONS {
            let oldest = tables
                .sessions
                .iter()
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
        address_may_try(&mut self.lock().attempts, address, now)
    }

    fn attempt_at(&self, name: &str, address: Option<IpAddr>, now: Instant) -> bool {
        let mut tables = self.lock();
        if !address_may_try(&mut tables.attempts, address, now) {
            return false;
        }
        // Also what keeps the table from growing with names nobody retries.
        tables
            .failures
            .retain(|_, failures| now.saturating_duration_since(failures.last) < LOCKOUT);
        let failures = tables
            .failures
            .entry((name.to_owned(), address))
            .or_insert(Failures {
                count: 0,
                last: now,
            });
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

/// Counts one attempt from `address`, unless it made
/// [`MAX_ATTEMPTS_PER_ADDRESS`] in the last minute, which returns false. A
/// connection without an address (the unix socket) is not limited: only the
/// owner of the daemon reaches it.
fn address_may_try(
    attempts: &mut HashMap<IpAddr, VecDeque<Instant>>,
    address: Option<IpAddr>,
    now: Instant,
) -> bool {
    let Some(address) = address else {
        return true;
    };
    let is_recent = |at: &Instant| now.saturating_duration_since(*at) < LOCKOUT;
    // Addresses that stopped trying leave the table.
    attempts.retain(|_, times| times.back().is_some_and(is_recent));
    let times = attempts.entry(address).or_default();
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
        let id = sessions.create_at("ana", None, start);
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
            .map(|index| sessions.create_at("ana", None, start + second * index))
            .collect();
        let now = start + second * 1000;
        assert!(sessions.get_at(&ids[0], now).is_some(), "a thousand fit");

        let newest = sessions.create_at("luis", None, now);
        assert!(sessions.get_at(&ids[0], now).is_none(), "the oldest went");
        assert!(sessions.get_at(&ids[1], now).is_some());
        assert!(sessions.get_at(&newest, now).is_some());
        assert_eq!(sessions.lock().sessions.len(), 1000);

        // Sessions that ended make room first.
        let later = start + LIFETIME + second * 500;
        sessions.create_at("eva", None, later);
        assert_eq!(sessions.lock().sessions.len(), 501);
    }

    #[test]
    fn ending_a_user_ends_all_its_sessions_and_no_other() {
        let sessions = Sessions::new();
        let (first, second) = (sessions.create("ana", None), sessions.create("ana", None));
        let other = sessions.create("luis", Some(vec![String::from("web")]));
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
        let ids: HashSet<String> = (0..100).map(|_| sessions.create("ana", None)).collect();
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
