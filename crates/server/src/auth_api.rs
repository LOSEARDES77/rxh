//! The `/auth` routes of the admin API: who the caller is, sign-in, sign-out,
//! creating the first user and changing one's password.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Response, StatusCode};
use serde::Deserialize;
use serde_json::json;

use crate::access::{ADMIN_ROLE, Scope};
use crate::admin::{self, AdminState, Caller, Peer};
use crate::session;

/// Longest name a user can have, in bytes. A longer one is no user: it is
/// refused before it costs a hash or a place among the failed sign-ins.
const MAX_NAME_BYTES: usize = 64;

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
            "password_login": password_login(peer),
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
    if !password_login(peer) {
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

/// `POST /auth/logout`: ends the session of the caller.
pub(crate) fn logout(state: &AdminState, caller: &Caller, peer: Peer) -> Response<Full<Bytes>> {
    if let Caller::User { name, session, .. } = caller {
        state.sessions.end(session);
        println!("Admin => {name} signed out from {}", client(peer));
    }
    no_content(session::expired_cookie(peer.tls))
}

/// `POST /auth/setup`: creates the first user, an administrator, and signs
/// it in. Only while no user exists, and only for an administrator (the
/// token, or a local request without a token) or for whoever sends the
/// setup code where [`admin::code_opens_setup`] holds.
pub(crate) async fn setup(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    if !state.access.read().user.is_empty() {
        return admin::error(StatusCode::CONFLICT, SETUP_DONE);
    }
    let body: SetupBody = match admin::read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
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
    let created = {
        let (state, name) = (Arc::clone(state), name.clone());
        // Hashing takes a quarter of a second: not on an async worker.
        tokio::task::spawn_blocking(move || {
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
        Err(_) => admin::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the user was not created",
        ),
    }
}

/// `PUT /auth/password`: a signed-in user replaces its own password, which
/// ends its sessions and starts a new one for this browser.
pub(crate) async fn password(
    state: &Arc<AdminState>,
    caller: &Caller,
    peer: Peer,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let Caller::User { name, .. } = caller else {
        return admin::error(
            StatusCode::FORBIDDEN,
            "only a signed-in user changes its password",
        );
    };
    let PasswordBody { current, new } = match admin::read_json(request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match check_password(state, name, current, peer).await {
        Ok(true) => {}
        // Not `401`, which tells the page that the session ended.
        Ok(false) => {
            println!(
                "Admin => {name} gave a wrong current password from {}",
                client(peer)
            );
            return admin::error(StatusCode::FORBIDDEN, "the current password is wrong");
        }
        Err(response) => return *response,
    }
    let changed = {
        let (state, name) = (Arc::clone(state), name.clone());
        tokio::task::spawn_blocking(move || {
            state
                .access
                .change(|access| access.set_password(&name, &new))
        })
        .await
    };
    match changed {
        Ok(Ok(())) => {
            state.sessions.end_user(name);
            println!("Admin => {name} changed its password from {}", client(peer));
            signed_in(state, name, peer)
        }
        Ok(Err(message)) => admin::error(StatusCode::BAD_REQUEST, &message),
        Err(_) => admin::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the password was not changed",
        ),
    }
}

/// Whether a session may start on this connection: a password and the
/// cookie it buys do not travel in clear text over the network.
fn password_login(peer: Peer) -> bool {
    peer.tls || peer.local
}

/// Whether `password` is the one of user `name`. `Err` with a `429` while
/// the name is locked out for this client. The attempt is counted as failed
/// before the check, which takes long enough for many others to start, and
/// forgotten when the password is right.
async fn check_password(
    state: &Arc<AdminState>,
    name: &str,
    password: String,
    peer: Peer,
) -> Result<bool, Box<Response<Full<Bytes>>>> {
    if state.sessions.locked(name, peer.address) {
        println!(
            "Admin => Sign-in as {name:?} refused from {}: too many failures",
            client(peer)
        );
        return Err(Box::new(admin::error(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed sign-ins for this name; try again in a minute",
        )));
    }
    state.sessions.failed(name, peer.address);
    let access = state.access.read();
    let user = name.to_owned();
    // bcrypt takes a quarter of a second: not on an async worker.
    let right = tokio::task::spawn_blocking(move || access.verify(&user, &password))
        .await
        .unwrap_or(false);
    if right {
        state.sessions.forget(name, peer.address);
    }
    Ok(right)
}

/// `204` with the cookie of a new session of `name`.
fn signed_in(state: &AdminState, name: &str, peer: Peer) -> Response<Full<Bytes>> {
    let id = state.sessions.create(name, None);
    no_content(session::cookie(&id, peer.tls))
}

fn no_content(cookie: HeaderValue) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::NO_CONTENT;
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
}

/// The client of a connection, for the log.
fn client(peer: Peer) -> String {
    peer.address.map_or_else(
        || String::from("the unix socket"),
        |address| address.to_string(),
    )
}
