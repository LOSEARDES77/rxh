//! A stand-in for an OpenID Connect provider, for the tests of the sign-in
//! through a provider: discovery, an authorization endpoint that always
//! says yes, and a token endpoint that checks what a real one checks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A wrong answer of the provider, see [`OidcProvider::fail_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcFault {
    /// The token endpoint answers `200` without an `id_token`.
    NoIdToken,
    /// The token endpoint answers after half a minute.
    Slow,
    /// The token endpoint answers with two mebibytes.
    Huge,
    /// The discovery document names another issuer.
    OtherIssuer,
    /// The discovery document names a token endpoint over plain HTTP on a
    /// host that is not loopback.
    HttpEndpoint,
}

/// What the authorization endpoint remembers for a code.
#[derive(Debug)]
struct Grant {
    challenge: String,
    nonce: String,
    redirect_uri: String,
}

#[derive(Debug)]
struct State {
    issuer: String,
    /// Claims that replace the default ones; `null` removes one.
    claims: Mutex<Value>,
    token_error: Mutex<Option<u16>>,
    fault: Mutex<Option<OidcFault>>,
    /// Codes issued and not exchanged yet.
    codes: Mutex<HashMap<String, Grant>>,
    /// Every code and token issued.
    secrets: Mutex<Vec<String>>,
    token_requests: AtomicUsize,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An OpenID Connect provider over plain HTTP on a local TCP address, with
/// one client: [`OidcProvider::CLIENT_ID`] and
/// [`OidcProvider::CLIENT_SECRET`].
#[derive(Debug)]
pub struct OidcProvider {
    state: Arc<State>,
}

impl OidcProvider {
    /// The client registered at the provider.
    pub const CLIENT_ID: &'static str = "rxh";

    /// Its secret, with characters that have to be encoded on the way.
    pub const CLIENT_SECRET: &'static str = "s3cret/of+the=client";

    /// Starts the provider on a free local port. Its ID tokens carry
    /// `iss`, `aud`, `sub` (`subject-1`), `exp` five minutes ahead and the
    /// `nonce` of the request, until [`OidcProvider::set_claims`].
    ///
    /// # Panics
    ///
    /// Panics if no socket can be bound to localhost.
    pub async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let state = Arc::new(State {
            issuer: format!("http://{address}"),
            claims: Mutex::new(json!({})),
            token_error: Mutex::new(None),
            fault: Mutex::new(None),
            codes: Mutex::default(),
            secrets: Mutex::default(),
            token_requests: AtomicUsize::new(0),
        });
        let shared = Arc::clone(&state);
        tokio::task::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::task::spawn(serve(stream, Arc::clone(&shared)));
            }
        });
        Self { state }
    }

    /// The `issuer` to give to `[admin.oidc]`.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.state.issuer
    }

    /// Puts the members of `claims` in the ID tokens issued from now on,
    /// over the default ones. A `null` member removes that claim.
    pub fn set_claims(&self, claims: Value) {
        *lock(&self.state.claims) = claims;
    }

    /// Makes the token endpoint answer `status` with an OAuth error body.
    pub fn set_token_error(&self, status: u16) {
        *lock(&self.state.token_error) = Some(status);
    }

    /// Makes the provider answer wrongly from now on.
    pub fn fail_with(&self, fault: OidcFault) {
        *lock(&self.state.fault) = Some(fault);
    }

    /// Plays the browser of a user who signs in at the provider: takes the
    /// `Location` the client redirected to and returns the callback URL the
    /// provider would send the browser back to, with a new code.
    ///
    /// # Panics
    ///
    /// Panics if the authorization request is not one this provider
    /// accepts: its endpoint, `response_type=code`, the client, a
    /// `redirect_uri`, the `openid` scope, `state`, `nonce` and an `S256`
    /// challenge.
    #[must_use]
    pub fn authorize(&self, location: &str) -> String {
        let endpoint = format!("{}/authorize?", self.state.issuer);
        let query = location
            .strip_prefix(&endpoint)
            .unwrap_or_else(|| panic!("{location} is not the authorization endpoint"));
        grant(&self.state, query).unwrap_or_else(|reason| panic!("{reason}: {location}"))
    }

    /// Every code and token the provider has issued.
    #[must_use]
    pub fn secrets(&self) -> Vec<String> {
        lock(&self.state.secrets).clone()
    }

    /// Requests the token endpoint has received.
    #[must_use]
    pub fn token_requests(&self) -> usize {
        self.state.token_requests.load(Ordering::Relaxed)
    }
}

/// The pairs of a query or a form body, decoded.
fn pairs(text: &str) -> HashMap<String, String> {
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (decode(name), decode(value)))
        .collect()
}

/// Decodes `application/x-www-form-urlencoded` text.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| text.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match (escaped, bytes[at]) {
            (Some(byte), _) => {
                decoded.push(byte);
                at += 2;
            }
            (None, b'+') => decoded.push(b' '),
            (None, byte) => decoded.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Encodes `text` for a query: everything but letters and digits.
fn encode(text: &str) -> String {
    let mut encoded = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The authorization endpoint: checks the request in `query`, remembers a
/// new code for it and returns the callback URL.
fn grant(state: &State, query: &str) -> Result<String, String> {
    let request = pairs(query);
    let get = |name: &str| {
        request
            .get(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("the authorization request has no {name}"))
    };
    if get("response_type")? != "code" {
        return Err(String::from("response_type is not code"));
    }
    if get("client_id")? != OidcProvider::CLIENT_ID {
        return Err(String::from("unknown client_id"));
    }
    if !get("scope")?.split(' ').any(|scope| scope == "openid") {
        return Err(String::from("the scope has no openid"));
    }
    if get("code_challenge_method")? != "S256" {
        return Err(String::from("code_challenge_method is not S256"));
    }
    let redirect_uri = get("redirect_uri")?.clone();
    let request_state = get("state")?;
    let granted = Grant {
        challenge: get("code_challenge")?.clone(),
        nonce: get("nonce")?.clone(),
        redirect_uri: redirect_uri.clone(),
    };
    let code = {
        let mut secrets = lock(&state.secrets);
        let code = format!("code-{}-of-the-provider", secrets.len());
        secrets.push(code.clone());
        code
    };
    lock(&state.codes).insert(code.clone(), granted);
    Ok(format!(
        "{redirect_uri}?code={code}&state={}",
        encode(request_state)
    ))
}

/// The client of a token request, from its `Authorization: Basic` header.
fn is_the_client(authorization: Option<&str>) -> bool {
    let credentials = authorization
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| STANDARD.decode(encoded.trim()).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok());
    credentials
        .as_deref()
        .and_then(|credentials| credentials.split_once(':'))
        .is_some_and(|(id, secret)| {
            decode(id) == OidcProvider::CLIENT_ID && decode(secret) == OidcProvider::CLIENT_SECRET
        })
}

/// The token endpoint: status and JSON body.
fn token(state: &State, authorization: Option<&str>, body: &str) -> (u16, Value) {
    if let Some(status) = *lock(&state.token_error) {
        return (status, json!({ "error": "server_error" }));
    }
    if !is_the_client(authorization) {
        return (401, json!({ "error": "invalid_client" }));
    }
    let form = pairs(body);
    let field = |name: &str| form.get(name).map_or("", String::as_str);
    let invalid = (400, json!({ "error": "invalid_grant" }));
    if field("grant_type") != "authorization_code" {
        return (400, json!({ "error": "unsupported_grant_type" }));
    }
    // A code is spent by the first request that names it.
    let Some(granted) = lock(&state.codes).remove(field("code")) else {
        return invalid;
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(field("code_verifier").as_bytes()));
    if granted.redirect_uri != field("redirect_uri") || granted.challenge != challenge {
        return invalid;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let mut claims = json!({
        "iss": state.issuer,
        "aud": OidcProvider::CLIENT_ID,
        "sub": "subject-1",
        "iat": now,
        "exp": now + 300,
        "nonce": granted.nonce,
    });
    if let (Some(claims), Some(replaced)) =
        (claims.as_object_mut(), lock(&state.claims).as_object())
    {
        for (name, value) in replaced {
            if value.is_null() {
                claims.remove(name);
            } else {
                claims.insert(name.clone(), value.clone());
            }
        }
    }
    let part = |value: &Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let id_token = format!(
        "{}.{}.{}",
        part(&json!({ "alg": "RS256", "typ": "JWT" })),
        part(&claims),
        URL_SAFE_NO_PAD.encode("a-signature-nobody-checks"),
    );
    let access_token = {
        let mut secrets = lock(&state.secrets);
        let access_token = format!("access-token-{}-of-the-provider", secrets.len());
        secrets.push(access_token.clone());
        secrets.push(id_token.clone());
        access_token
    };
    let mut answer = json!({
        "access_token": access_token,
        "token_type": "Bearer",
        "expires_in": 300,
        "id_token": id_token,
    });
    match *lock(&state.fault) {
        Some(OidcFault::NoIdToken) => {
            answer["id_token"].take();
        }
        Some(OidcFault::Huge) => answer["padding"] = Value::from("x".repeat(2 * 1024 * 1024)),
        Some(OidcFault::Slow | OidcFault::OtherIssuer | OidcFault::HttpEndpoint) | None => {}
    }
    (200, answer)
}

/// The discovery document.
fn discovery(state: &State) -> Value {
    let issuer = &state.issuer;
    let fault = *lock(&state.fault);
    json!({
        "issuer": match fault {
            Some(OidcFault::OtherIssuer) => "https://another-issuer.example",
            _ => issuer,
        },
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": match fault {
            Some(OidcFault::HttpEndpoint) => String::from("http://provider.example/token"),
            _ => format!("{issuer}/token"),
        },
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["RS256"],
    })
}

async fn serve(mut stream: TcpStream, state: Arc<State>) {
    let mut request: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => return,
        }
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (found, value) = line.split_once(':')?;
            found.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    };
    let length: usize = header("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    while request.len() < head_end + length {
        match stream.read(&mut chunk).await {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => return,
        }
    }
    let body = String::from_utf8_lossy(&request[head_end..]).into_owned();
    let mut line = head.lines().next().unwrap_or_default().split(' ');
    let (method, target) = (
        line.next().unwrap_or_default(),
        line.next().unwrap_or_default(),
    );
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let response = match (method, path) {
        ("GET", "/.well-known/openid-configuration") => json_answer(200, &discovery(&state)),
        ("GET", "/authorize") => match grant(&state, query) {
            Ok(callback) => format!(
                "HTTP/1.1 302 Found\r\nLocation: {callback}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            ),
            Err(reason) => json_answer(400, &json!({ "error": reason })),
        },
        ("POST", "/token") => {
            state.token_requests.fetch_add(1, Ordering::Relaxed);
            if *lock(&state.fault) == Some(OidcFault::Slow) {
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
            let (status, answer) = token(&state, header("authorization"), &body);
            json_answer(status, &answer)
        }
        _ => json_answer(404, &json!({ "error": "not found" })),
    };
    let _ = stream.write_all(response.as_bytes()).await;
}

fn json_answer(status: u16, body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status} Answer\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// For a run by hand against a real `rxh`: prints the issuer and the
    /// client, signs in the user of `OIDC_CLAIMS` (JSON), and serves until
    /// it is killed. `cargo test -p rxh-test-util -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "serves until it is killed, for a run by hand"]
    async fn serve_a_provider_by_hand() {
        let provider = OidcProvider::start().await;
        if let Ok(claims) = std::env::var("OIDC_CLAIMS") {
            provider.set_claims(serde_json::from_str(&claims).expect("OIDC_CLAIMS is JSON"));
        }
        println!("issuer = {:?}", provider.issuer());
        println!("client_id = {:?}", OidcProvider::CLIENT_ID);
        println!("client_secret = {:?}", OidcProvider::CLIENT_SECRET);
        std::future::pending::<()>().await;
    }

    #[tokio::test]
    async fn a_code_is_exchanged_once_and_only_with_its_verifier() {
        let provider = OidcProvider::start().await;
        let state = &provider.state;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(b"the-verifier"));
        let location = format!(
            "{}/authorize?response_type=code&client_id=rxh&redirect_uri=http%3A%2F%2Fapp%2Fcb\
             &scope=openid%20email&state=s&nonce=n&code_challenge={challenge}\
             &code_challenge_method=S256",
            provider.issuer()
        );
        let basic = |secret: &str| format!("Basic {}", STANDARD.encode(format!("rxh:{secret}")));
        let right = basic(&encode(OidcProvider::CLIENT_SECRET));
        let form = |callback: &str, verifier: &str| {
            let code = callback.split("code=").nth(1).expect("a code");
            let code = code.split('&').next().expect("a code");
            format!(
                "grant_type=authorization_code&code={code}&redirect_uri=http%3A%2F%2Fapp%2Fcb\
                 &code_verifier={verifier}"
            )
        };

        let callback = provider.authorize(&location);
        assert!(callback.starts_with("http://app/cb?code="), "{callback}");
        assert!(callback.ends_with("&state=s"), "{callback}");
        let body = form(&callback, "the-verifier");
        assert_eq!(token(state, Some(&basic("wrong")), &body).0, 401);
        assert_eq!(token(state, None, &body).0, 401);
        let (status, answer) = token(state, Some(&right), &body);
        assert_eq!(status, 200, "{answer}");
        assert!(
            answer["id_token"]
                .as_str()
                .expect("a token")
                .split('.')
                .count()
                == 3
        );
        assert_eq!(token(state, Some(&right), &body).0, 400, "used once");

        let callback = provider.authorize(&location);
        let wrong = form(&callback, "another-verifier");
        assert_eq!(token(state, Some(&right), &wrong).0, 400);
        // A wrong verifier spends the code too.
        let body = form(&callback, "the-verifier");
        assert_eq!(token(state, Some(&right), &body).0, 400);
    }
}
