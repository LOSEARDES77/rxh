//! The `/auth` routes of the admin API: who the caller is, sign-in, sign-out,
//! creating the first user and changing one's password.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::OwnedSemaphorePermit;

use crate::access::{ADMIN_ROLE, Scope};
use crate::admin::{self, AdminState, Caller, Peer};
use crate::session;

/// Longest name a user can have, in bytes. A longer one is no user: it is
/// refused before it costs a hash or a place among the failed sign-ins.
const MAX_NAME_BYTES: usize = 64;

/// Time a request waits for its turn to hash or compare a password, so that
/// a burst delays the others instead of refusing them.
#[cfg(not(test))]
const HASH_WAIT: Duration = Duration::from_secs(5);
/// Short, so that the tests of a wait that ends in a refusal stay fast.
#[cfg(test)]
const HASH_WAIT: Duration = Duration::from_millis(300);

/// Answer of `POST /auth/setup` once a user exists.
const SETUP_DONE: &str = "a user exists already; sign in";

#[derive(Deserialize)]
struct LoginBody {
    name: String,
    password: String,
}

#[derive(Deserialize)]
struct SetupBody {
    name: String,
    password: String,
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct PasswordBody {
    current: String,
    new: String,
}

/// `GET /auth/session`: who the caller is and what the page may offer.
pub(crate) fn session(state: &AdminState, caller: &Caller, peer: Peer) -> Response<Full<Bytes>> {
    let access = state.access.read();
    let setup = access.user.is_empty();
    let (user, roles) = match caller {
        Caller::User { name, session, .. } => {
            let roles = state
                .sessions
                .get(session)
                .and_then(|session| admin::roles(&access, &session))
                .unwrap_or_default();
            (Some(name.as_str()), roles)
        }
        Caller::Token | Caller::Local | Caller::Nobody => (None, Vec::new()),
    };
    // An administrator has no grants to list: `admin` says everything.
    let scope = caller.scope();
    let grants = match &scope {
        Scope::Grants(grants) => grants.as_slice(),
        Scope::Admin => &[],
    };
    let needs_code =
        setup && !caller.is_admin() && admin::code_opens_setup(peer, state.token.is_some());
    admin::json_response(
        StatusCode::OK,
        &json!({
            "user": user,
            "admin": caller.is_admin(),
            "roles": roles,
            "grants": grants,
            "creatable_groups": scope.creatable_groups(),
            "setup": setup,
            "setup_needs_code": needs_code,
            "password_login": peer.may_hold_session(),
            "oidc": false,
        }),
    )
}

/// `POST /auth/login`: checks a name and its password and starts a session.
pub(crate) async fn login(
    state: &Arc<AdminState>,
    peer: Peer,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    if !peer.may_hold_session() {
        return admin::error(
            StatusCode::FORBIDDEN,
            "signing in with a password needs HTTPS or a connection from this machine",
        );
    }
    let LoginBody { name, password } = match admin::read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let wrong = || admin::error(StatusCode::UNAUTHORIZED, "wrong name or password");
    if name.len() > MAX_NAME_BYTES {
        println!(
            "Admin => Failed sign-in with a name that is too long from {}",
            client(peer)
        );
        return wrong();
    }
    match check_password(state, &name, password, peer).await {
        Ok(true) => {
            println!("Admin => {name} signed in from {}", client(peer));
            signed_in(state, &name, peer)
        }
        // An unknown name and a wrong password get the same answer.
        Ok(false) => {
            println!("Admin => Failed sign-in as {name:?} from {}", client(peer));
            wrong()
        }
        Err(response) => *response,
    }
}

/// `POST /auth/logout`: ends the session the cookie names and makes the
/// browser drop the cookie. For anyone: a session whose user was removed
/// belongs to nobody and still has to end.
pub(crate) fn logout<B>(
    state: &AdminState,
    request: &Request<B>,
    peer: Peer,
) -> Response<Full<Bytes>> {
    if let Some(id) = session::cookie_of(request) {
        // The rule of every cookie request that changes something.
        if !admin::origin_is_host(request) {
            return admin::error(StatusCode::FORBIDDEN, admin::COOKIE_NEEDS_ORIGIN);
        }
        if let Some(session) = state.sessions.get(id) {
            state.sessions.end(id);
            println!("Admin => {} signed out from {}", session.user, client(peer));
        }
    }
    no_content(Some(session::expired_cookie(peer.tls)))
}

/// `POST /auth/setup`: see [`create_first_user`].
pub(crate) async fn setup(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    match admin::read_json(request).await {
        Ok(body) => create_first_user(state, caller, peer, body).await,
        Err(response) => *response,
    }
}

/// Creates the first user, an administrator, and signs it in. Only while no
/// user exists, and only for an administrator (the token, or a local
/// request without a token) or for whoever sends the setup code where
/// [`admin::code_opens_setup`] holds.
async fn create_first_user(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    body: SetupBody,
) -> Response<Full<Bytes>> {
    if !state.access.read().user.is_empty() {
        return admin::error(StatusCode::CONFLICT, SETUP_DONE);
    }
    // A guess of the code is a sign-in attempt of its address.
    if body.code.is_some() && !state.sessions.guess(peer.address) {
        println!(
            "Admin => Setup refused from {}: too many attempts",
            client(peer)
        );
        return too_many_attempts();
    }
    let has_code = admin::code_opens_setup(peer, state.token.is_some())
        && body.code.as_deref().is_some_and(|code| {
            aws_lc_rs::constant_time::verify_slices_are_equal(
                code.as_bytes(),
                state.sessions.setup_code().as_bytes(),
            )
            .is_ok()
        });
    let creator = match (caller.is_admin(), has_code) {
        (true, _) => caller.label(),
        (false, true) => "the setup code",
        (false, false) => {
            return admin::error(
                StatusCode::FORBIDDEN,
                "creating the first user needs an administrator or the setup code",
            );
        }
    };

    let SetupBody { name, password, .. } = body;
    let permit = match hash_permit(state).await {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let created = {
        let (state, name) = (Arc::clone(state), name.clone());
        hashed(permit, move || {
            state.access.change(|access| {
                // Checked again here, where no other change can interleave.
                if !access.user.is_empty() {
                    return Err(SETUP_DONE.to_owned());
                }
                access.add_user(&name, &password, &[ADMIN_ROLE.to_owned()])
            })
        })
        .await
    };
    match created {
        Ok(Ok(())) => {
            println!(
                "Admin => First user {name} created by {creator} from {}",
                client(peer)
            );
            signed_in(state, &name, peer)
        }
        Ok(Err(message)) if message == SETUP_DONE => admin::error(StatusCode::CONFLICT, SETUP_DONE),
        Ok(Err(message)) => admin::error(StatusCode::BAD_REQUEST, &message),
        Err(response) => *response,
    }
}

/// `PUT /auth/password`: see [`change_password`].
pub(crate) async fn password(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    match admin::read_json(request).await {
        Ok(body) => change_password(state, caller, peer, body).await,
        Err(response) => *response,
    }
}

/// A signed-in user replaces its own password, which ends its sessions and
/// starts a new one for this browser.
async fn change_password(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    body: PasswordBody,
) -> Response<Full<Bytes>> {
    let Caller::User { name, .. } = caller else {
        return admin::error(
            StatusCode::FORBIDDEN,
            "only a signed-in user changes its password",
        );
    };
    if let Err(response) = count_attempt(state, name, peer) {
        return *response;
    }
    let permit = match hash_permit(state).await {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let PasswordBody { current, new } = body;
    // The comparison and the new hash, one after the other on one permit.
    // `Ok(false)` when `current` is wrong.
    let changed = {
        let (state, name) = (Arc::clone(state), name.clone());
        hashed(permit, move || {
            if !state.access.read().verify(&name, &current) {
                return Ok(false);
            }
            state
                .access
                .change(|access| access.set_password(&name, &new))
                .map(|()| true)
        })
        .await
    };
    match changed {
        Ok(Ok(true)) => {
            state.sessions.forget(name, peer.address);
            state.sessions.end_user(name);
            println!("Admin => {name} changed its password from {}", client(peer));
            signed_in(state, name, peer)
        }
        // Not `401`, which tells the page that the session ended.
        Ok(Ok(false)) => {
            println!(
                "Admin => {name} gave a wrong current password from {}",
                client(peer)
            );
            admin::error(StatusCode::FORBIDDEN, "the current password is wrong")
        }
        // The current password was right, the new one is not accepted.
        Ok(Err(message)) => {
            state.sessions.forget(name, peer.address);
            admin::error(StatusCode::BAD_REQUEST, &message)
        }
        Err(response) => *response,
    }
}

/// The right to hash or compare one password, waited for up to
/// [`HASH_WAIT`]. `Err` with a `429` when none came free in that time.
async fn hash_permit(
    state: &AdminState,
) -> Result<OwnedSemaphorePermit, Box<Response<Full<Bytes>>>> {
    let permit = Arc::clone(&state.hashing).acquire_owned();
    match tokio::time::timeout(HASH_WAIT, permit).await {
        Ok(Ok(permit)) => Ok(permit),
        // The semaphore is never closed: this is the wait that ran out.
        Ok(Err(_)) | Err(_) => Err(Box::new(admin::error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many passwords are being checked; try again in a moment",
        ))),
    }
}

/// Runs `work`, which hashes or compares a password, off the async workers:
/// bcrypt takes a quarter of a second. The permit is held until the work
/// ends, also when the request that asked for it is gone. `Err` with a
/// `500` when the work panicked.
async fn hashed<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let done = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await;
    done.map_err(|err| {
        eprintln!("Admin => A password was not checked: {err}");
        Box::new(admin::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the password was not checked",
        ))
    })
}

/// Counts a sign-in as `name` from the client of `peer` before anything is
/// hashed for it. `Err` with a `429` when the client tried too often in the
/// last minute, or failed too often under this name.
fn count_attempt(
    state: &AdminState,
    name: &str,
    peer: Peer,
) -> Result<(), Box<Response<Full<Bytes>>>> {
    if state.sessions.attempt(name, peer.address) {
        return Ok(());
    }
    println!(
        "Admin => Sign-in as {name:?} refused from {}: too many attempts",
        client(peer)
    );
    Err(Box::new(too_many_attempts()))
}

fn too_many_attempts() -> Response<Full<Bytes>> {
    admin::error(
        StatusCode::TOO_MANY_REQUESTS,
        "too many sign-in attempts; try again in a minute",
    )
}

/// Whether `password` is the one of user `name`. `Err` with a `429` when
/// the attempt is refused, see [`count_attempt`], or no permit came free.
/// The attempt is counted as failed before the check, which takes long
/// enough for many others to start, and forgotten when the password is
/// right.
async fn check_password(
    state: &Arc<AdminState>,
    name: &str,
    password: String,
    peer: Peer,
) -> Result<bool, Box<Response<Full<Bytes>>>> {
    count_attempt(state, name, peer)?;
    let permit = hash_permit(state).await?;
    let access = state.access.read();
    let user = name.to_owned();
    let right = hashed(permit, move || access.verify(&user, &password)).await?;
    if right {
        state.sessions.forget(name, peer.address);
    }
    Ok(right)
}

/// `204`, with the cookie of a new session of `name` where a session may
/// be: the token on plain HTTP from another machine gets none, which would
/// travel in clear text.
fn signed_in(state: &AdminState, name: &str, peer: Peer) -> Response<Full<Bytes>> {
    if !peer.may_hold_session() {
        return no_content(None);
    }
    let id = state.sessions.create(name, None);
    no_content(Some(session::cookie(&id, peer.tls)))
}

fn no_content(cookie: Option<HeaderValue>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::NO_CONTENT;
    if let Some(cookie) = cookie {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

/// The client of a connection, for the log.
fn client(peer: Peer) -> String {
    peer.address.map_or_else(
        || String::from("the unix socket"),
        |address| address.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::access::Access;
    use crate::admin::tests::{LOCAL, REMOTE, state};

    const TOKEN: &str = "0123456789abcdef";

    /// Plain HTTP from another machine.
    const PLAIN: Peer = Peer {
        tls: false,
        ..REMOTE
    };

    /// A daemon without users, and the directory of its users file.
    fn daemon(token: Option<&str>) -> (Arc<AdminState>, TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let users = directory.path().join("users.toml");
        (Arc::new(state(token, users)), directory)
    }

    fn body(code: Option<&str>) -> SetupBody {
        SetupBody {
            name: String::from("ana"),
            password: String::from("password-ana"),
            code: code.map(str::to_owned),
        }
    }

    fn users(directory: &Path) -> Access {
        Access::load(&directory.join("users.toml")).unwrap()
    }

    #[tokio::test]
    async fn the_setup_code_creates_the_first_administrator_over_tls() {
        let (state, directory) = daemon(None);
        let code = state.sessions.setup_code().to_owned();
        let response = create_first_user(&state, &Caller::Nobody, REMOTE, body(Some(&code))).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(cookie.ends_with("; Secure"), "{cookie}");
        let saved = users(directory.path());
        assert_eq!(saved.user.len(), 1);
        assert_eq!(saved.user[0].name, "ana");
        assert_eq!(saved.user[0].roles, [ADMIN_ROLE]);
        assert!(saved.verify("ana", "password-ana"));

        // The code is of no use once a user exists.
        let mut second = body(Some(&code));
        second.name = String::from("eva");
        let response = create_first_user(&state, &Caller::Nobody, REMOTE, second).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(users(directory.path()).user.len(), 1);
    }

    #[tokio::test]
    async fn setup_without_the_right_code_creates_nothing() {
        let (open, directory) = daemon(None);
        let code = open.sessions.setup_code().to_owned();
        // The code with another last character.
        let last = if code.ends_with('x') { 'y' } else { 'x' };
        let almost = format!("{}{last}", &code[..code.len() - 1]);
        for (peer, given) in [
            (REMOTE, None),
            (REMOTE, Some("")),
            (REMOTE, Some(almost.as_str())),
            (REMOTE, Some(&code[..9])),
            // The right code, in clear text.
            (PLAIN, Some(code.as_str())),
        ] {
            let response = create_first_user(&open, &Caller::Nobody, peer, body(given)).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{given:?}");
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            assert!(users(directory.path()).user.is_empty(), "{given:?}");
        }

        // With a token the code opens nothing, and the token does.
        let (closed, directory) = daemon(Some(TOKEN));
        let code = closed.sessions.setup_code().to_owned();
        let response = create_first_user(&closed, &Caller::Nobody, REMOTE, body(Some(&code))).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(users(directory.path()).user.is_empty());
        let response = create_first_user(&closed, &Caller::Token, REMOTE, body(None)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(users(directory.path()).user.len(), 1);
    }

    #[tokio::test]
    async fn no_cookie_is_given_on_plain_http_from_another_machine() {
        let (closed, directory) = daemon(Some(TOKEN));
        let response = create_first_user(&closed, &Caller::Token, PLAIN, body(None)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(users(directory.path()).user.len(), 1, "the user is created");

        let response = signed_in(&closed, "ana", PLAIN);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        for peer in [LOCAL, REMOTE] {
            let response = signed_in(&closed, "ana", peer);
            assert!(response.headers().contains_key(header::SET_COOKIE));
        }
    }

    /// A daemon with the user `ana`, an administrator.
    fn daemon_with_ana() -> (Arc<AdminState>, TempDir) {
        let (state, directory) = daemon(None);
        state
            .access
            .change(|access| access.add_user("ana", "password-ana", &[ADMIN_ROLE.to_owned()]))
            .unwrap();
        (state, directory)
    }

    /// Takes every hashing permit, as four slow sign-ins would.
    fn keep_busy(state: &AdminState) -> OwnedSemaphorePermit {
        Arc::clone(&state.hashing)
            .try_acquire_many_owned(4)
            .unwrap()
    }

    async fn check(
        state: &Arc<AdminState>,
        password: &str,
        peer: Peer,
    ) -> Result<bool, StatusCode> {
        check_password(state, "ana", password.to_owned(), peer)
            .await
            .map_err(|response| response.status())
    }

    #[tokio::test]
    async fn a_sign_in_waits_for_its_turn_to_hash() {
        let (state, _directory) = daemon_with_ana();

        // Four at it, and the fifth comes: it waits, then is answered.
        let busy = keep_busy(&state);
        let release = tokio::task::spawn(async move {
            tokio::time::sleep(HASH_WAIT / 6).await;
            drop(busy);
        });
        let started = std::time::Instant::now();
        assert_eq!(check(&state, "password-ana", REMOTE).await, Ok(true));
        assert!(started.elapsed() >= HASH_WAIT / 6, "it waited");
        release.await.unwrap();
        assert_eq!(check(&state, "not-the-password", REMOTE).await, Ok(false));
        assert_eq!(state.hashing.available_permits(), 4, "each one is returned");

        // Nothing frees in time: refused, after the wait.
        let busy = keep_busy(&state);
        let started = std::time::Instant::now();
        assert_eq!(
            check(&state, "password-ana", REMOTE).await,
            Err(StatusCode::TOO_MANY_REQUESTS)
        );
        assert!(started.elapsed() >= HASH_WAIT);
        drop(busy);

        // A hash that panics is an error, not a wrong password.
        let permit = hash_permit(&state).await.unwrap();
        let failed = hashed(permit, || -> bool { panic!("as a broken hash would") }).await;
        assert_eq!(
            failed.map_err(|response| response.status()),
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        );
        assert_eq!(state.hashing.available_permits(), 4);
    }

    #[tokio::test]
    async fn setup_and_password_change_wait_for_their_turn_too() {
        // Setup: no hash, no user.
        let (empty, directory) = daemon(Some(TOKEN));
        let busy = keep_busy(&empty);
        let response = create_first_user(&empty, &Caller::Token, REMOTE, body(None)).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(users(directory.path()).user.is_empty());
        drop(busy);
        let response = create_first_user(&empty, &Caller::Token, REMOTE, body(None)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // Password change: the password and the sessions stay.
        let (state, directory) = daemon_with_ana();
        let session = state.sessions.create("ana", None);
        let ana = Caller::User {
            name: String::from("ana"),
            scope: Scope::Admin,
            session: session.clone(),
        };
        let change = async |current: &str, new: &str| {
            let body = PasswordBody {
                current: current.to_owned(),
                new: new.to_owned(),
            };
            change_password(&state, &ana, REMOTE, body).await
        };
        let busy = keep_busy(&state);
        let response = change("password-ana", "another-password").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert!(users(directory.path()).verify("ana", "password-ana"));
        assert!(state.sessions.get(&session).is_some());
        drop(busy);

        let response = change("not-the-password", "another-password").await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = change("password-ana", "short").await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(users(directory.path()).verify("ana", "password-ana"));
        let response = change("password-ana", "another-password").await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().contains_key(header::SET_COOKIE));
        assert!(users(directory.path()).verify("ana", "another-password"));
        assert!(state.sessions.get(&session).is_none(), "the old session");
        assert_eq!(state.hashing.available_permits(), 4);
    }

    #[tokio::test]
    async fn an_address_that_changes_names_is_refused_from_the_eleventh_attempt() {
        let (state, _directory) = daemon_with_ana();
        let other = Peer {
            address: Some("192.0.2.9".parse().unwrap()),
            ..REMOTE
        };
        let socket = Peer {
            local: true,
            tls: false,
            address: None,
        };
        for index in 0..10 {
            let name = format!("user{index}");
            let answer = check_password(&state, &name, String::from("a-password"), REMOTE).await;
            assert!(matches!(answer, Ok(false)), "attempt {index}");
        }
        // Whatever the name now, and with the right password too, without
        // a hash: no permit is free and it does not wait for one.
        let busy = keep_busy(&state);
        let started = std::time::Instant::now();
        assert_eq!(
            check(&state, "password-ana", REMOTE).await,
            Err(StatusCode::TOO_MANY_REQUESTS)
        );
        assert!(started.elapsed() < HASH_WAIT, "refused at once");
        drop(busy);
        let unknown = check_password(&state, "user10", String::from("a-password"), REMOTE).await;
        assert_eq!(
            unknown.map_err(|response| response.status()),
            Err(StatusCode::TOO_MANY_REQUESTS)
        );

        // Another address signs in, and the unix socket has no limit.
        assert_eq!(check(&state, "password-ana", other).await, Ok(true));
        for index in 0..12 {
            let name = format!("user{index}");
            let answer = check_password(&state, &name, String::from("a-password"), socket).await;
            assert!(matches!(answer, Ok(false)), "attempt {index}");
        }
        assert_eq!(check(&state, "password-ana", socket).await, Ok(true));
    }

    #[tokio::test]
    async fn guesses_of_the_setup_code_count_as_attempts_of_the_address() {
        let (state, directory) = daemon(None);
        let code = state.sessions.setup_code().to_owned();
        for attempt in 0..10 {
            let wrong = body(Some("aaaaaaaaaa"));
            let response = create_first_user(&state, &Caller::Nobody, REMOTE, wrong).await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "attempt {attempt}"
            );
        }
        // The right code now is not even compared.
        let response = create_first_user(&state, &Caller::Nobody, REMOTE, body(Some(&code))).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(users(directory.path()).user.is_empty());

        let other = Peer {
            address: Some("192.0.2.9".parse().unwrap()),
            ..REMOTE
        };
        let response = create_first_user(&state, &Caller::Nobody, other, body(Some(&code))).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(users(directory.path()).user.len(), 1);
    }

    #[tokio::test]
    async fn signing_out_ends_the_session_of_the_cookie_for_anyone() {
        let (state, _directory) = daemon(None);
        // No user `eva` in the file: the session is nobody's.
        let id = state.sessions.create("eva", None);
        let with = |origin: Option<&str>| {
            let mut builder = Request::builder()
                .header(header::HOST, "rxh.example:2019")
                .header(header::COOKIE, format!("rxh_session={id}"));
            if let Some(origin) = origin {
                builder = builder.header(header::ORIGIN, origin);
            }
            builder.body(()).unwrap()
        };
        for origin in [None, Some("https://evil.example")] {
            let response = logout(&state, &with(origin), REMOTE);
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{origin:?}");
            assert!(state.sessions.get(&id).is_some(), "{origin:?}");
        }
        let response = logout(&state, &with(Some("https://rxh.example:2019")), REMOTE);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()[header::SET_COOKIE],
            "rxh_session=; HttpOnly; SameSite=Strict; Path=/; Secure; Max-Age=0"
        );
        assert!(state.sessions.get(&id).is_none());

        // Without a cookie there is nothing to end, and nothing to refuse.
        let response = logout(&state, &Request::new(()), LOCAL);
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}
