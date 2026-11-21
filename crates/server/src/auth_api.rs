//! The `/auth` routes of the admin API: who the caller is, sign-in, sign-out,
//! creating the first user and changing one's password; and the `/users`
//! and `/roles` routes, with which an administrator manages the accounts.

use std::borrow::Cow;
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

use crate::access::{ADMIN_ROLE, Access, Grant, Role, Scope};
use crate::admin::{self, AdminState, Caller, Peer};
use crate::oidc;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUserBody {
    name: String,
    password: String,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UserChangeBody {
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    password: Option<String>,
}

/// A role as `PUT /roles/{name}` takes it: the name is the one of the path.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    grant: Vec<Grant>,
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
    let issuer = state.oidc.as_ref().map(oidc::Client::issuer_host);
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
            "oidc": issuer.is_some(),
            "oidc_issuer": issuer,
            // A user of the users file, who has a password to change.
            "local": matches!(caller, Caller::User { oidc: false, .. }),
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
    let checked = check_password(state, &name, password, peer).await;
    match checked.map(|hash| hash.and_then(|hash| signed_in_with(state, &name, &hash, peer))) {
        Ok(Some(response)) => {
            println!("Admin => {name} signed in from {}", client(peer));
            response
        }
        // An unknown name and a wrong password get the same answer, and so
        // does a password changed or a user removed during the check.
        Ok(None) => {
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
        (false, true) => Cow::Borrowed("the setup code"),
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
    let Caller::User { name, oidc, .. } = caller else {
        return admin::error(
            StatusCode::FORBIDDEN,
            "only a signed-in user changes its password",
        );
    };
    // Its name may be the one of a user of the users file, who is somebody
    // else: refused before anything is compared or counted under that name.
    if *oidc {
        return admin::error(
            StatusCode::FORBIDDEN,
            "this account signs in through the provider",
        );
    }
    if let Err(response) = count_attempt(state, name, peer) {
        return *response;
    }
    let permit = match attempt_permit(state, name, peer).await {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let PasswordBody { current, new } = body;
    // The comparison and the new hash, one after the other on one permit.
    // `Ok(Some(new hash))` once changed, `Ok(None)` when `current` is wrong
    // or the password changed since it was compared.
    let changed = {
        let (state, name) = (Arc::clone(state), name.clone());
        hashed(permit, move || {
            let compared = state.access.read();
            if !compared.verify(&name, &current) {
                return Ok(None);
            }
            let Some(hash) = stored_hash(&compared, &name).map(str::to_owned) else {
                return Ok(None);
            };
            state.access.change(|access| {
                if !holds(access, &name, &hash) {
                    return Ok(None);
                }
                access.set_password(&name, &new)?;
                Ok(stored_hash(access, &name).map(str::to_owned))
            })
        })
        .await
    };
    match changed {
        Ok(Ok(Some(hash))) => {
            state.sessions.forget(name, peer.address);
            state.sessions.end_user(name);
            println!("Admin => {name} changed its password from {}", client(peer));
            // Changed again or removed since: no new session.
            signed_in_with(state, name, &hash, peer).unwrap_or_else(|| no_content(None))
        }
        // Not `401`, which tells the page that the session ended.
        Ok(Ok(None)) => {
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

/// A response, or the one that ended the request early.
type Answer = Result<Response<Full<Bytes>>, Box<Response<Full<Bytes>>>>;

/// `GET /users`: every user with its roles.
pub(crate) fn list_users(state: &AdminState) -> Response<Full<Bytes>> {
    let users: Vec<_> = state
        .access
        .read()
        .user
        .iter()
        .map(|user| json!({ "name": user.name, "roles": user.roles }))
        .collect();
    admin::json_response(StatusCode::OK, &users)
}

/// `POST /users`: adds a user with a password and roles.
pub(crate) async fn create_user(
    state: &Arc<AdminState>,
    caller: &Caller,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let created = async {
        let NewUserBody {
            name,
            password,
            roles,
        } = admin::read_json(request).await?;
        let permit = hash_permit(state).await?;
        let (added, added_roles) = (name.clone(), roles.clone());
        change_access(state, Some(permit), None, move |access| {
            access.add_user(&added, &password, &added_roles)
        })
        .await?;
        println!("Admin => User {name} created by {}", caller.label());
        let user = json!({ "name": name, "roles": roles });
        Answer::Ok(admin::json_response(StatusCode::CREATED, &user))
    };
    created.await.unwrap_or_else(|response| *response)
}

/// `PUT /users/{name}`: replaces the roles of a user, its password or both.
/// A new password ends the sessions of the user.
pub(crate) async fn change_user(
    state: &Arc<AdminState>,
    caller: &Caller,
    name: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let changed = async {
        let UserChangeBody { roles, password } = admin::read_json(request).await?;
        if roles.is_none() && password.is_none() {
            return Err(Box::new(admin::error(
                StatusCode::BAD_REQUEST,
                "the body names roles, a password or both",
            )));
        }
        let new_password = password.is_some();
        let permit = if new_password {
            Some(hash_permit(state).await?)
        } else {
            None
        };
        let user = name.to_owned();
        change_access(state, permit, Some(missing_user(name)), move |access| {
            if let Some(roles) = &roles {
                access.set_roles(&user, roles)?;
            }
            if let Some(password) = &password {
                access.set_password(&user, password)?;
            }
            Ok(())
        })
        .await?;
        if new_password {
            state.sessions.end_user(name);
        }
        println!("Admin => User {name} changed by {}", caller.label());
        Answer::Ok(no_content(None))
    };
    changed.await.unwrap_or_else(|response| *response)
}

/// `DELETE /users/{name}`: removes a user and ends its sessions.
pub(crate) async fn remove_user(
    state: &Arc<AdminState>,
    caller: &Caller,
    name: &str,
) -> Response<Full<Bytes>> {
    let user = name.to_owned();
    let removed = change_access(state, None, Some(missing_user(name)), move |access| {
        access.remove_user(&user)
    })
    .await;
    if let Err(response) = removed {
        return *response;
    }
    state.sessions.end_user(name);
    println!("Admin => User {name} removed by {}", caller.label());
    no_content(None)
}

/// `GET /roles`: the built-in `admin` role, then every defined role.
pub(crate) fn list_roles(state: &AdminState) -> Response<Full<Bytes>> {
    let mut roles = vec![json!({ "name": ADMIN_ROLE, "builtin": true })];
    for role in state.access.read().role {
        roles.push(json!(role));
    }
    admin::json_response(StatusCode::OK, &roles)
}

/// `POST /roles`: defines a role whose name is not taken.
pub(crate) async fn create_role(
    state: &Arc<AdminState>,
    caller: &Caller,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let created = async {
        let role: Role = admin::read_json(request).await?;
        let added = role.clone();
        change_access(state, None, None, move |access| {
            if access.role.iter().any(|other| other.name == added.name) {
                return Err(format!("role {:?} exists", added.name));
            }
            access.put_role(added)
        })
        .await?;
        println!("Admin => Role {} created by {}", role.name, caller.label());
        Answer::Ok(admin::json_response(StatusCode::CREATED, &role))
    };
    created.await.unwrap_or_else(|response| *response)
}

/// `PUT /roles/{name}`: replaces the grants of a defined role.
pub(crate) async fn replace_role(
    state: &Arc<AdminState>,
    caller: &Caller,
    name: &str,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let replaced = async {
        let RoleBody { name: named, grant } = admin::read_json(request).await?;
        if named.is_some_and(|named| named != name) {
            return Err(Box::new(admin::error(
                StatusCode::BAD_REQUEST,
                "the name in the body is not the one in the path",
            )));
        }
        let role = Role {
            name: name.to_owned(),
            grant,
        };
        change_access(state, None, Some(missing_role(name)), move |access| {
            // `admin` goes on to the rules, which say it is built in.
            if role.name != ADMIN_ROLE && !access.role.iter().any(|other| other.name == role.name) {
                return Err(missing_role(&role.name));
            }
            access.put_role(role)
        })
        .await?;
        println!("Admin => Role {name} replaced by {}", caller.label());
        Answer::Ok(no_content(None))
    };
    replaced.await.unwrap_or_else(|response| *response)
}

/// `DELETE /roles/{name}`: removes a role that no user has.
pub(crate) async fn remove_role(
    state: &Arc<AdminState>,
    caller: &Caller,
    name: &str,
) -> Response<Full<Bytes>> {
    let role = name.to_owned();
    let removed = change_access(state, None, Some(missing_role(name)), move |access| {
        access.remove_role(&role)
    })
    .await;
    if let Err(response) = removed {
        return *response;
    }
    println!("Admin => Role {name} removed by {}", caller.label());
    no_content(None)
}

/// The reason [`Access`] gives for a user that does not exist.
fn missing_user(name: &str) -> String {
    format!("no user {name:?}")
}

/// The reason [`Access`] gives for a role that does not exist.
fn missing_role(name: &str) -> String {
    format!("no role {name:?}")
}

/// Applies `edit` to the users file off the async workers, where it may
/// wait for a change that hashes, holding `permit` when `edit` hashes a
/// password. A refusal is `404` when its reason is `missing`, else `409`.
async fn change_access(
    state: &Arc<AdminState>,
    permit: Option<OwnedSemaphorePermit>,
    missing: Option<String>,
    edit: impl FnOnce(&mut Access) -> Result<(), String> + Send + 'static,
) -> Result<(), Box<Response<Full<Bytes>>>> {
    let store = Arc::clone(state);
    let changed = off_workers(permit, move || store.access.change(edit)).await?;
    changed.map_err(|reason| {
        let status = if missing.as_deref() == Some(reason.as_str()) {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::CONFLICT
        };
        Box::new(admin::error(status, &reason))
    })
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

/// [`hash_permit`] for a sign-in as `name` that [`count_attempt`] counted.
/// A wait that runs out takes the failure of the name back: no password was
/// compared, and whoever keeps the permits busy would lock the name out.
async fn attempt_permit(
    state: &AdminState,
    name: &str,
    peer: Peer,
) -> Result<OwnedSemaphorePermit, Box<Response<Full<Bytes>>>> {
    let permit = hash_permit(state).await;
    if permit.is_err() {
        state.sessions.uncount(name, peer.address);
    }
    permit
}

/// Runs `work`, which hashes or compares a password, off the async workers:
/// bcrypt takes a quarter of a second. See [`off_workers`].
async fn hashed<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    off_workers(Some(permit), work).await
}

/// Runs `work` on a thread that may block, holding `permit` until the work
/// ends, also when the request that asked for it is gone. `Err` with a
/// `500` when the work panicked.
async fn off_workers<T: Send + 'static>(
    permit: Option<OwnedSemaphorePermit>,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Box<Response<Full<Bytes>>>> {
    let done = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await;
    done.map_err(|err| {
        eprintln!("Admin => A request was not completed: {err}");
        Box::new(admin::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the request was not completed",
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

/// Answer to a client that tried to sign in too often in the last minute.
pub(crate) const TOO_MANY_ATTEMPTS: &str = "too many sign-in attempts; try again in a minute";

fn too_many_attempts() -> Response<Full<Bytes>> {
    admin::error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY_ATTEMPTS)
}

/// Whether `password` is the one of user `name`: `Some` with the hash it
/// matched, `None` when it is wrong. `Err` with a `429` when the attempt is
/// refused, see [`count_attempt`], or no permit came free. The attempt is
/// counted as failed before the check, which takes long enough for many
/// others to start, and forgotten when the password is right.
async fn check_password(
    state: &Arc<AdminState>,
    name: &str,
    password: String,
    peer: Peer,
) -> Result<Option<String>, Box<Response<Full<Bytes>>>> {
    count_attempt(state, name, peer)?;
    let permit = attempt_permit(state, name, peer).await?;
    let access = state.access.read();
    let user = name.to_owned();
    // `verify` first: an unknown name costs a comparison too.
    let matched = hashed(permit, move || {
        let right = access.verify(&user, &password);
        right
            .then(|| stored_hash(&access, &user).map(str::to_owned))
            .flatten()
    })
    .await?;
    if matched.is_some() {
        state.sessions.forget(name, peer.address);
    }
    Ok(matched)
}

/// The password hash of user `name` in `access`.
fn stored_hash<'access>(access: &'access Access, name: &str) -> Option<&'access str> {
    access
        .user
        .iter()
        .find(|user| user.name == name)
        .map(|user| user.password.as_str())
}

/// Whether user `name` still has the password `hash` in `access`: neither
/// changed nor removed since a password was compared with it.
fn holds(access: &Access, name: &str, hash: &str) -> bool {
    stored_hash(access, name) == Some(hash)
}

/// [`signed_in`] for a password that matched `hash`, or `None` when the
/// users file no longer gives `name` that hash. A new password or a removal
/// saved while the password was compared ended the sessions of the user
/// before this one existed, so the file is read after the session is made:
/// a change saved later ends it in turn.
fn signed_in_with(
    state: &AdminState,
    name: &str,
    hash: &str,
    peer: Peer,
) -> Option<Response<Full<Bytes>>> {
    if !peer.may_hold_session() {
        return holds(&state.access.read(), name, hash).then(|| no_content(None));
    }
    let id = state.sessions.create(name, None, Some(hash));
    if !holds(&state.access.read(), name, hash) {
        state.sessions.end(&id);
        return None;
    }
    Some(no_content(Some(session::cookie(&id, peer.tls))))
}

/// `204`, with the cookie of a new session of `name` where a session may
/// be: the token on plain HTTP from another machine gets none, which would
/// travel in clear text.
fn signed_in(state: &AdminState, name: &str, peer: Peer) -> Response<Full<Bytes>> {
    if !peer.may_hold_session() {
        return no_content(None);
    }
    let hash = stored_hash(&state.access.read(), name).map(str::to_owned);
    let id = state.sessions.create(name, None, hash.as_deref());
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
pub(crate) fn client(peer: Peer) -> String {
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

    #[tokio::test]
    async fn behind_a_port_forwarder_setup_needs_the_code_and_tls() {
        use http_body_util::BodyExt;

        let (open, directory) = daemon(None);
        let code = open.sessions.setup_code().to_owned();
        // Every client looks like this machine there, and is nobody.
        let plain = admin::tests::forwarded();
        let over_tls = Peer { tls: true, ..plain };
        for (peer, password_login, needs_code) in [(plain, false, false), (over_tls, true, true)] {
            let response = session(&open, &Caller::Nobody, peer);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["setup"], true);
            assert_eq!(json["admin"], false);
            assert_eq!(json["password_login"], password_login);
            assert_eq!(json["setup_needs_code"], needs_code);
        }
        for (peer, given) in [
            (plain, None),
            (over_tls, None),
            // The right code, in clear text.
            (plain, Some(code.as_str())),
        ] {
            let response = create_first_user(&open, &Caller::Nobody, peer, body(given)).await;
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{given:?}");
            assert!(users(directory.path()).user.is_empty(), "{given:?}");
        }
        let response = create_first_user(&open, &Caller::Nobody, over_tls, body(Some(&code))).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(users(directory.path()).user.len(), 1);

        // No session starts in clear text, even with the right password.
        let response = signed_in(&open, "ana", plain);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        let hash = stored_hash(&open.access.read(), "ana").unwrap().to_owned();
        let response = signed_in_with(&open, "ana", &hash, plain).unwrap();
        assert!(!response.headers().contains_key(header::SET_COOKIE));
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
            .map(|hash| hash.is_some())
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
    async fn a_wait_that_runs_out_is_not_a_failed_sign_in_of_the_name() {
        let (state, _directory) = daemon_with_ana();
        // Five sign-ins while every permit is taken: none compares anything.
        let busy = keep_busy(&state);
        let refused = tokio::join!(
            check(&state, "password-ana", REMOTE),
            check(&state, "password-ana", REMOTE),
            check(&state, "password-ana", REMOTE),
            check(&state, "password-ana", REMOTE),
            check(&state, "password-ana", REMOTE),
        );
        let too_many = Err(StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(refused.0, too_many);
        assert_eq!(refused.4, too_many);
        drop(busy);
        // The name is not locked out: five failures would have done that.
        assert_eq!(check(&state, "password-ana", REMOTE).await, Ok(true));

        // A wrong password is still a failure of the name.
        let other = Peer {
            address: Some("192.0.2.9".parse().unwrap()),
            ..REMOTE
        };
        for _ in 0..5 {
            assert_eq!(check(&state, "not-the-password", other).await, Ok(false));
        }
        assert_eq!(check(&state, "password-ana", other).await, too_many);
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
        let hash = stored_hash(&state.access.read(), "ana").unwrap().to_owned();
        let session = state.sessions.create("ana", None, Some(&hash));
        let ana = Caller::User {
            name: String::from("ana"),
            scope: Scope::Admin,
            session: session.clone(),
            oidc: false,
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
            assert!(matches!(answer, Ok(None)), "attempt {index}");
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
            assert!(matches!(answer, Ok(None)), "attempt {index}");
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
    async fn a_password_changed_or_a_user_removed_during_the_check_starts_no_session() {
        let (state, _directory) = daemon_with_ana();
        let compared = state.access.read();
        let hash = stored_hash(&compared, "ana").unwrap().to_owned();
        assert!(holds(&compared, "ana", &hash));
        let response = signed_in_with(&state, "ana", &hash, LOCAL).unwrap();
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        let id = cookie["rxh_session=".len()..].split(';').next().unwrap();
        assert!(state.sessions.get(id).is_some());

        // An administrator sets a new password while the old one is compared.
        state
            .access
            .change(|access| access.set_password("ana", "another-password"))
            .unwrap();
        assert!(!holds(&state.access.read(), "ana", &hash));
        for peer in [LOCAL, REMOTE, PLAIN] {
            assert!(signed_in_with(&state, "ana", &hash, peer).is_none());
        }

        // Removed, then created again with the old password: another hash.
        let current = stored_hash(&state.access.read(), "ana").unwrap().to_owned();
        state
            .access
            .change(|access| {
                access.add_user("eva", "password-eva", &[ADMIN_ROLE.to_owned()])?;
                access.remove_user("ana")
            })
            .unwrap();
        assert!(!holds(&state.access.read(), "ana", &current));
        assert!(signed_in_with(&state, "ana", &current, REMOTE).is_none());
        state
            .access
            .change(|access| access.add_user("ana", "password-ana", &[]))
            .unwrap();
        assert!(signed_in_with(&state, "ana", &hash, REMOTE).is_none());
        let current = stored_hash(&state.access.read(), "ana").unwrap().to_owned();
        assert!(signed_in_with(&state, "ana", &current, REMOTE).is_some());
    }

    #[tokio::test]
    async fn signing_out_ends_the_session_of_the_cookie_for_anyone() {
        let (state, _directory) = daemon(None);
        // No user `eva` in the file: the session is nobody's.
        let id = state.sessions.create("eva", None, None);
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
