//! Sign-in through an OpenID Connect provider (`[admin.oidc]`): discovery,
//! the authorization code flow with PKCE, `state`, `nonce` and a cookie
//! that binds the flow to the browser that started it, the claims of the
//! ID token, the roles its groups give, and the two routes of the flow.
//!
//! The signature of the ID token is not checked: the token is read only
//! from the answer of the token endpoint, over TLS, which OpenID Connect
//! Core 3.1.3.7 allows. A token from anywhere else is never looked at.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::client::conn::http1;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use rxh_core::config::{self, UpstreamGroup, UpstreamTls};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::admin::{self, AdminState, Peer};
use crate::admin_guard::AdminGuard;
use crate::auth_api;
use crate::files;
use crate::session;
use crate::stream::Dialer;

/// Time the provider has to answer one request, connection included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest answer read from the provider, in bytes.
const MAX_ANSWER: usize = 1024 * 1024;

/// Time the discovery document is kept before it is read again.
const DISCOVERY_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// Time a flow has from `GET /auth/oidc/login` to its callback.
const FLOW_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Flows under way kept at most; the oldest goes when one more starts.
const MAX_FLOWS: usize = 1000;

/// Seconds the clock of the provider may be behind this one.
const CLOCK_SKEW: f64 = 60.0;

/// Longest name a user of the provider can have, in bytes.
const MAX_NAME_BYTES: usize = 256;

/// Groups of a user that a page or a log line names, and the bytes of each.
const MAX_GROUPS_SHOWN: usize = 20;
const MAX_GROUP_BYTES: usize = 64;

/// Name of the cookie that binds a flow to the browser that started it.
const BINDING_COOKIE: &str = "rxh_oidc";

/// Answer to a callback whose flow is not the one of its browser: no such
/// `state`, one that was used or is too old, or another binding cookie.
const NOT_THIS_BROWSER: &str =
    "this sign-in was not started in this browser, was used already or took too long; start again";

/// The endpoints of the provider, from its discovery document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoints {
    authorization: String,
    token: String,
}

/// A flow between `GET /auth/oidc/login` and its callback.
struct Flow {
    /// Value of the cookie given to the browser that started the flow.
    binding: String,
    nonce: String,
    /// The PKCE verifier.
    verifier: String,
    created: Instant,
}

/// The client of one provider.
pub(crate) struct Client {
    settings: config::Oidc,
    /// `{public_url}/auth/oidc/callback`.
    redirect_uri: String,
    /// `{public_url}/dashboard`.
    dashboard: String,
    issuer_host: String,
    /// Connections to `https` endpoints, and to `http` ones on loopback.
    tls: Dialer,
    plain: Dialer,
    /// The endpoints and when they were read.
    discovery: Mutex<Option<(Instant, Endpoints)>>,
    /// Flows under way by `state`.
    pending: Mutex<HashMap<String, Flow>>,
}

/// Where to send the browser to sign in, and the cookie to give it.
pub(crate) struct Start {
    pub(crate) location: String,
    pub(crate) binding: String,
}

/// Who signed in at the provider.
pub(crate) struct Identity {
    pub(crate) name: String,
    pub(crate) roles: Vec<String>,
    pub(crate) groups: Vec<String>,
}

/// Why a sign-in did not happen. The texts hold nothing secret: they are
/// shown to the browser and logged.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The provider did not answer, or not what the protocol says.
    Provider(String),
    /// The callback or the ID token is not one to accept.
    Refused(String),
    /// The user is who the provider says, and no group of it gives a role.
    NoRole { name: String, groups: Vec<String> },
}

impl Client {
    /// The client for `settings`, whose callback is under `public_url`.
    /// Fails when the TLS settings cannot be built, with an error that
    /// names `[admin.oidc]`.
    pub(crate) fn new(
        settings: &config::Oidc,
        public_url: &str,
        guard: AdminGuard,
    ) -> io::Result<Self> {
        Self::build(settings, public_url, guard)
            .map_err(|err| io::Error::new(err.kind(), format!("[admin.oidc]: {err}")))
    }

    fn build(settings: &config::Oidc, public_url: &str, guard: AdminGuard) -> io::Result<Self> {
        let dialer = |tls: Option<UpstreamTls>| {
            let group = UpstreamGroup {
                dial_timeout: REQUEST_TIMEOUT,
                tls,
                ..UpstreamGroup::default()
            };
            Dialer::new(&group, guard.clone())
        };
        let issuer: Uri = settings.issuer.parse().map_err(io::Error::other)?;
        Ok(Self {
            settings: settings.clone(),
            redirect_uri: format!("{public_url}/auth/oidc/callback"),
            dashboard: format!("{public_url}/dashboard"),
            issuer_host: issuer.host().unwrap_or_default().to_owned(),
            tls: dialer(Some(UpstreamTls {
                ca: settings.ca.clone(),
                ..UpstreamTls::default()
            }))?,
            plain: dialer(None)?,
            discovery: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Starts a flow: reads the discovery document unless it is fresh, and
    /// builds the URL of the authorization endpoint for a new `state`,
    /// `nonce` and PKCE verifier, which wait for the callback.
    pub(crate) async fn start(&self) -> Result<Start, Failure> {
        let endpoints = self.endpoints().await?;
        let (state, nonce, verifier, binding) = (random(), random(), random(), random());
        let separator = if endpoints.authorization.contains('?') {
            '&'
        } else {
            '?'
        };
        let location = format!(
            "{}{separator}response_type=code&client_id={}&redirect_uri={}&scope={}\
             &state={state}&nonce={nonce}&code_challenge={}&code_challenge_method=S256",
            endpoints.authorization,
            encode(&self.settings.client_id),
            encode(&self.redirect_uri),
            encode(&self.settings.scopes.join(" ")),
            challenge(&verifier),
        );
        let flow = Flow {
            binding: binding.clone(),
            nonce,
            verifier,
            created: Instant::now(),
        };
        remember(&mut self.pending(), state, flow);
        Ok(Start { location, binding })
    }

    /// Ends the flow a callback names in its `query`: takes the flow of its
    /// `state`, which must be the one of the browser (`binding` is its
    /// cookie), exchanges the code for an ID token at the token endpoint,
    /// checks the claims and maps the groups to roles.
    pub(crate) async fn finish(
        &self,
        query: &str,
        binding: Option<&str>,
    ) -> Result<Identity, Failure> {
        let value = |name: &str| {
            query
                .split('&')
                .filter_map(|pair| pair.split_once('='))
                .find(|(found, _)| *found == name)
                .map(|(_, value)| files::percent_decode(&value.replace('+', " ")))
        };
        // First of all: nothing a callback says counts until it is known to
        // come from the browser that started the flow.
        let flow = value("state")
            .and_then(|state| take(&mut self.pending(), &state, binding, Instant::now()))
            .ok_or_else(|| Failure::Refused(NOT_THIS_BROWSER.to_owned()))?;
        if let Some(error) = value("error") {
            return Err(Failure::Refused(format!(
                "the provider did not sign the user in{}",
                named(&error)
            )));
        }
        let code = value("code")
            .filter(|code| !code.is_empty())
            .ok_or_else(|| Failure::Refused(String::from("the provider sent no code")))?;

        let endpoints = self.endpoints().await?;
        // RFC 6749 2.3.1: both encoded as a form encodes them, then Basic.
        let credentials = STANDARD.encode(format!(
            "{}:{}",
            encode(&self.settings.client_id),
            encode(&self.settings.client_secret)
        ));
        let form = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&code_verifier={}",
            encode(&code),
            encode(&self.redirect_uri),
            flow.verifier,
        );
        let headers = [
            (header::AUTHORIZATION, format!("Basic {credentials}")),
            (
                header::CONTENT_TYPE,
                String::from("application/x-www-form-urlencoded"),
            ),
        ];
        let (status, body) = self
            .request(&endpoints.token, Method::POST, &headers, form)
            .await
            .map_err(Failure::Provider)?;
        let answer: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if status != StatusCode::OK {
            let error = answer["error"].as_str().unwrap_or_default();
            return Err(Failure::Provider(format!(
                "the token endpoint of the provider answered {status}{}",
                named(error)
            )));
        }
        let id_token = answer["id_token"].as_str().ok_or_else(|| {
            Failure::Provider(String::from("the answer of the provider has no ID token"))
        })?;
        let claims = claims_of(id_token).map_err(Failure::Provider)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let settings = &self.settings;
        check_claims(
            &claims,
            &settings.issuer,
            &settings.client_id,
            &flow.nonce,
            now,
        )
        .map_err(Failure::Refused)?;

        let name = name_of(&claims, &settings.name_claim)
            .ok_or_else(|| Failure::Refused(String::from("the ID token does not name the user")))?;
        if !is_shown_as_it_is(&name) {
            return Err(Failure::Refused(String::from(
                "the name of the user in the ID token has control characters or more than 256 \
                 bytes",
            )));
        }
        let groups = groups_of(&claims, &settings.groups_claim);
        let roles = roles_of(&groups, &settings.roles);
        if roles.is_empty() {
            return Err(Failure::NoRole { name, groups });
        }
        Ok(Identity {
            name,
            roles,
            groups,
        })
    }

    /// Host of the issuer, for the page to say where the sign-in goes.
    pub(crate) fn issuer_host(&self) -> &str {
        &self.issuer_host
    }

    /// The endpoints of the provider: the ones read less than an hour ago,
    /// else the discovery document is read again.
    async fn endpoints(&self) -> Result<Endpoints, Failure> {
        let now = Instant::now();
        let cached = self.discovery().clone();
        if let Some((read, endpoints)) = cached
            && now.saturating_duration_since(read) < DISCOVERY_LIFETIME
        {
            return Ok(endpoints);
        }
        let url = discovery_url(&self.settings.issuer);
        let (status, body) = self
            .request(&url, Method::GET, &[], String::new())
            .await
            .map_err(Failure::Provider)?;
        if status != StatusCode::OK {
            return Err(Failure::Provider(format!(
                "the discovery document of the provider answered {status}"
            )));
        }
        let endpoints = endpoints_of(&body, &self.settings.issuer).map_err(Failure::Provider)?;
        *self.discovery() = Some((now, endpoints.clone()));
        Ok(endpoints)
    }

    /// Sends one HTTP/1 request to the provider and reads its answer: over
    /// TLS with the trust roots of upstream TLS, or in clear to a loopback
    /// address, never to the admin API of this process, within
    /// [`REQUEST_TIMEOUT`] and up to [`MAX_ANSWER`] bytes. Redirects are
    /// not followed. The error is a sentence without the request in it.
    async fn request(
        &self,
        url: &str,
        method: Method,
        headers: &[(HeaderName, String)],
        body: String,
    ) -> Result<(StatusCode, Bytes), String> {
        let not_secure = || String::from("an address of the provider is not https");
        let uri: Uri = url.parse().map_err(|_| not_secure())?;
        let (Some(scheme), Some(authority), true) =
            (uri.scheme_str(), uri.authority(), is_secure(url))
        else {
            return Err(not_secure());
        };
        let dialer = if scheme == "https" {
            &self.tls
        } else {
            &self.plain
        };
        let host = authority.host();
        let target = uri.path_and_query().map_or("/", |target| target.as_str());
        let mut request = Request::builder()
            .method(method)
            .uri(target)
            .header(header::HOST, authority.as_str())
            .header(header::ACCEPT, "application/json")
            .header(header::CONNECTION, "close");
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let request = request
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| not_secure())?;

        // `None` for an answer that is too long.
        let exchange = async {
            let stream = dialer.dial(&format!("{scheme}://{authority}")).await?;
            let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
                .await
                .map_err(io::Error::other)?;
            tokio::task::spawn(connection);
            let response = sender
                .send_request(request)
                .await
                .map_err(io::Error::other)?;
            let status = response.status();
            match Limited::new(response.into_body(), MAX_ANSWER)
                .collect()
                .await
            {
                Ok(body) => Ok(Some((status, body.to_bytes()))),
                Err(err) if err.is::<LengthLimitError>() => Ok(None),
                Err(err) => Err(io::Error::other(err)),
            }
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, exchange).await {
            Ok(Ok(Some(answer))) => Ok(answer),
            Ok(Ok(None)) => Err(format!("the answer of {host} is too long")),
            // The page is for anybody: what went wrong stays in the log.
            Ok(Err(err)) => {
                eprintln!("Admin => The provider at {host} could not be reached: {err}");
                Err(format!("{host} could not be reached"))
            }
            Err(_) => Err(format!("{host} did not answer in time")),
        }
    }

    fn discovery(&self) -> MutexGuard<'_, Option<(Instant, Endpoints)>> {
        self.discovery
            .lock()
            .expect("nothing panics while holding the discovery document")
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<String, Flow>> {
        self.pending
            .lock()
            .expect("nothing panics while holding the pending flows")
    }
}

/// 32 random bytes in base64url: 43 characters.
fn random() -> String {
    let mut bytes = [0_u8; 32];
    aws_lc_rs::rand::fill(&mut bytes).expect("the random generator of the system works");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `text` for a query or a form: everything but the unreserved characters
/// of RFC 3986 as `%XX`.
fn encode(text: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").expect("writing to a string does not fail");
        }
    }
    encoded
}

/// ` (code)` for an error code of the provider that is a plain word, as
/// the ones of OAuth are; nothing for any other text, which is not shown.
fn named(error: &str) -> String {
    let is_word = !error.is_empty()
        && error.len() <= 64
        && error
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if is_word {
        format!(" ({error})")
    } else {
        String::new()
    }
}

/// Where the discovery document of `issuer` is.
fn discovery_url(issuer: &str) -> String {
    format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    )
}

/// True if `url` is `https`, or `http` on a loopback address, and holds no
/// user or password.
fn is_secure(url: &str) -> bool {
    let Ok(uri) = url.parse::<Uri>() else {
        return false;
    };
    let host = uri.host().unwrap_or_default();
    let is_loopback = host
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    let has_user = uri
        .authority()
        .is_some_and(|authority| authority.as_str().contains('@'));
    match uri.scheme_str() {
        Some("https") => !host.is_empty() && !has_user,
        Some("http") => is_loopback && !has_user,
        _ => false,
    }
}

/// The endpoints a discovery document names, for the provider configured
/// as `issuer`: the document must be of that issuer, exactly, and its
/// endpoints `https`. Only a provider that is itself `http` on a loopback
/// address may name such endpoints: one over `https` does not get the
/// client secret sent in clear to a port of this machine.
fn endpoints_of(document: &[u8], issuer: &str) -> Result<Endpoints, String> {
    let document: Value = serde_json::from_slice(document)
        .map_err(|_| String::from("the discovery document of the provider is not JSON"))?;
    if document["issuer"].as_str() != Some(issuer) {
        return Err(String::from(
            "the discovery document of the provider is of another issuer than the configured one",
        ));
    }
    let plain_issuer = issuer.starts_with("http://");
    let endpoint = |name: &str| {
        document[name]
            .as_str()
            .filter(|url| is_secure(url) && (plain_issuer || url.starts_with("https://")))
            .map(str::to_owned)
            .ok_or_else(|| {
                format!("the discovery document of the provider has a {name} that is not https")
            })
    };
    Ok(Endpoints {
        authorization: endpoint("authorization_endpoint")?,
        token: endpoint("token_endpoint")?,
    })
}

/// Keeps `flow` until the callback with `state`, dropping the flows that
/// are too old and, at [`MAX_FLOWS`], the oldest.
fn remember(flows: &mut HashMap<String, Flow>, state: String, flow: Flow) {
    let now = flow.created;
    flows.retain(|_, other| !too_old(other, now));
    if flows.len() >= MAX_FLOWS {
        let oldest = flows
            .iter()
            .min_by_key(|(_, other)| other.created)
            .map(|(state, _)| state.clone());
        if let Some(oldest) = oldest {
            flows.remove(&oldest);
        }
    }
    flows.insert(state, flow);
}

fn too_old(flow: &Flow, now: Instant) -> bool {
    now.saturating_duration_since(flow.created) >= FLOW_LIFETIME
}

/// Takes the flow of `state` out of `flows`, where it is no longer: a
/// state is used once. `None` when there is none, when it is older than
/// [`FLOW_LIFETIME`], or when `binding` is not the one of its browser.
fn take(
    flows: &mut HashMap<String, Flow>,
    state: &str,
    binding: Option<&str>,
    now: Instant,
) -> Option<Flow> {
    let flow = flows.remove(state)?;
    let is_of_browser = binding.is_some_and(|binding| {
        aws_lc_rs::constant_time::verify_slices_are_equal(
            binding.as_bytes(),
            flow.binding.as_bytes(),
        )
        .is_ok()
    });
    (is_of_browser && !too_old(&flow, now)).then_some(flow)
}

/// The claims of an ID token: the payload of a compact JWT, which is three
/// base64url parts. The signature is not checked, see the module.
fn claims_of(id_token: &str) -> Result<Value, String> {
    let malformed = || String::from("the ID token of the provider is not a signed JSON Web Token");
    let mut parts = id_token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(malformed());
    };
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| malformed())?;
    let claims: Value = serde_json::from_slice(&payload).map_err(|_| malformed())?;
    if claims.is_object() {
        Ok(claims)
    } else {
        Err(malformed())
    }
}

/// Checks the claims of an ID token that came from the token endpoint:
/// `iss` is the configured `issuer`, exactly; `aud` is `client_id` or a
/// list with it (and `azp` is `client_id` when the list has more, or when
/// there is one at all); `exp` is not more than a minute before `now`
/// (seconds since the epoch); `nonce` is the one of the flow.
fn check_claims(
    claims: &Value,
    issuer: &str,
    client_id: &str,
    nonce: &str,
    now: u64,
) -> Result<(), String> {
    if claims["iss"].as_str() != Some(issuer) {
        return Err(String::from(
            "the ID token is of another issuer than the configured one",
        ));
    }
    let is_client = |value: &Value| value.as_str() == Some(client_id);
    let azp = &claims["azp"];
    let for_client = match &claims["aud"] {
        Value::Array(audiences) => audiences.iter().any(is_client),
        audience => is_client(audience),
    };
    if !for_client {
        return Err(String::from("the ID token is for another audience"));
    }
    let needs_azp = claims["aud"].as_array().is_some_and(|list| list.len() > 1);
    if (needs_azp || !azp.is_null()) && !is_client(azp) {
        return Err(String::from(
            "the ID token is for another party (its azp is not this client)",
        ));
    }
    let Some(expiry) = claims["exp"].as_f64() else {
        return Err(String::from("the ID token has no expiry (exp)"));
    };
    #[expect(
        clippy::cast_precision_loss,
        reason = "seconds since the epoch are far below 2^53"
    )]
    if expiry + CLOCK_SKEW <= now as f64 {
        return Err(String::from("the ID token has expired"));
    }
    if claims["nonce"].as_str() != Some(nonce) {
        return Err(String::from("the ID token has another nonce"));
    }
    Ok(())
}

/// True for a name that can go to a page and to the log as it is: no
/// control character, which would start a line of its own in the log, and
/// at most [`MAX_NAME_BYTES`].
fn is_shown_as_it_is(name: &str) -> bool {
    name.len() <= MAX_NAME_BYTES && !name.chars().any(char::is_control)
}

/// `groups` for a page or the log: the first [`MAX_GROUPS_SHOWN`], each cut
/// at [`MAX_GROUP_BYTES`], and how many more there are.
fn shown(groups: &[String]) -> String {
    let cut = |group: &String| {
        if group.len() <= MAX_GROUP_BYTES {
            return group.clone();
        }
        let mut end = MAX_GROUP_BYTES;
        while !group.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\u{2026}", &group[..end])
    };
    let first: Vec<String> = groups.iter().take(MAX_GROUPS_SHOWN).map(cut).collect();
    let more = groups.len().saturating_sub(MAX_GROUPS_SHOWN);
    if more == 0 {
        first.join(", ")
    } else {
        format!("{} and {more} more", first.join(", "))
    }
}

/// The name of the user: the claim `name_claim`, else `sub`.
fn name_of(claims: &Value, name_claim: &str) -> Option<String> {
    [name_claim, "sub"]
        .into_iter()
        .filter_map(|claim| claims[claim].as_str())
        .find(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The groups of the user: the claim `groups_claim`, a string or a list
/// whose strings count.
fn groups_of(claims: &Value, groups_claim: &str) -> Vec<String> {
    match &claims[groups_claim] {
        Value::String(group) => vec![group.clone()],
        Value::Array(groups) => groups
            .iter()
            .filter_map(|group| group.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// The roles `mapping` gives to `groups`: their union, sorted, each once.
fn roles_of(groups: &[String], mapping: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let roles: BTreeSet<&String> = groups
        .iter()
        .filter_map(|group| mapping.get(group))
        .flatten()
        .collect();
    roles.into_iter().cloned().collect()
}

/// The PKCE challenge of `verifier` (RFC 7636, `S256`).
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `text` as HTML text or as the value of a quoted attribute.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The binding cookie a request carries.
pub(crate) fn binding_of<B>(request: &Request<B>) -> Option<&str> {
    session::cookie_named(request, BINDING_COOKIE)
}

/// The `Set-Cookie` of the binding cookie: `Lax`, so that the browser
/// sends it when the provider sends it back here. A `max_age` of zero
/// makes the browser drop it.
fn binding_cookie(binding: &str, secure: bool, max_age: u64) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    let value = format!(
        "{BINDING_COOKIE}={binding}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}"
    );
    HeaderValue::from_str(&value).expect("a binding is base64url, which a header can hold")
}

/// A small page without scripts: `sentence` and a link to the dashboard,
/// where the browser goes by itself when `refresh` is set.
fn page(
    status: StatusCode,
    title: &str,
    sentence: &str,
    dashboard: &str,
    refresh: bool,
) -> Response<Full<Bytes>> {
    let (sentence, dashboard) = (escape(sentence), escape(dashboard));
    let refresh = if refresh {
        format!("<meta http-equiv=\"refresh\" content=\"0; url={dashboard}\">\n")
    } else {
        String::new()
    };
    let body = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         {refresh}<title>{title}</title>\n</head>\n<body>\n<h1>{title}</h1>\n<p>{sentence}</p>\n\
         <p><a href=\"{dashboard}\">Go to the dashboard</a></p>\n</body>\n</html>\n"
    );
    let mut response = Response::new(Full::new(Bytes::from(body)));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/html; charset=utf-8"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// The status and the sentence that say why a sign-in did not happen.
fn described(failure: &Failure) -> (StatusCode, String) {
    match failure {
        Failure::Provider(reason) => (
            StatusCode::BAD_GATEWAY,
            format!("The sign-in could not be completed: {reason}."),
        ),
        Failure::Refused(reason) => (
            StatusCode::FORBIDDEN,
            format!("The sign-in was refused: {reason}."),
        ),
        Failure::NoRole { name, groups } => {
            let groups = if groups.is_empty() {
                String::from("it has none")
            } else {
                shown(groups)
            };
            (
                StatusCode::FORBIDDEN,
                format!(
                    "{name} signed in at the provider, but none of its groups ({groups}) gives \
                     a role here. Ask an administrator."
                ),
            )
        }
    }
}

/// The client of the provider, where the flow is offered: `404` without
/// `[admin.oidc]`, `403` where a session may not be held.
fn offered(state: &AdminState, peer: Peer) -> Result<&Client, Box<Response<Full<Bytes>>>> {
    let Some(client) = &state.oidc else {
        return Err(Box::new(admin::error(
            StatusCode::NOT_FOUND,
            "sign-in through a provider is not configured",
        )));
    };
    if !peer.may_hold_session() {
        return Err(Box::new(admin::error(
            StatusCode::FORBIDDEN,
            "signing in through the provider needs HTTPS or a connection from this machine",
        )));
    }
    Ok(client)
}

/// Counts the start of a flow among the sign-in attempts of its client:
/// each makes this process keep a flow and may make it call the provider.
/// `Err` with the `429` page once the client has made too many in the last
/// minute. The callback is not counted: it does nothing without a flow
/// that was.
fn counted(
    state: &AdminState,
    client: &Client,
    peer: Peer,
) -> Result<(), Box<Response<Full<Bytes>>>> {
    if state.sessions.guess(peer.address) {
        return Ok(());
    }
    println!(
        "Admin => Sign-in through the provider refused from {}: too many attempts",
        auth_api::client(peer)
    );
    Err(Box::new(page(
        StatusCode::TOO_MANY_REQUESTS,
        "Not signed in",
        &format!("The sign-in was refused: {}.", auth_api::TOO_MANY_ATTEMPTS),
        &client.dashboard,
        false,
    )))
}

/// Logs a sign-in that did not happen and answers with its page.
fn refused(client: &Client, peer: Peer, failure: &Failure) -> Response<Full<Bytes>> {
    let (status, sentence) = described(failure);
    // Names and groups are the provider's to choose: logged escaped.
    println!(
        "Admin => Sign-in through the provider refused from {}: {sentence:?}",
        auth_api::client(peer)
    );
    page(status, "Not signed in", &sentence, &client.dashboard, false)
}

/// True if the browser that sends `request` says it is not a person going
/// to a page of this API: `Sec-Fetch-Site` says the page of another site
/// asked for it, or `Sec-Fetch-Dest` that it is for an image, a frame or a
/// script. A client that sends neither header (`curl`) is not one a foreign
/// page can drive.
fn is_sent_by_another_page<B>(request: &Request<B>) -> bool {
    let value = |name: &str| request.headers().get(name).map(HeaderValue::as_bytes);
    value("sec-fetch-site") == Some(b"cross-site")
        || value("sec-fetch-dest").is_some_and(|destination| destination != b"document")
}

/// `GET /auth/oidc/login`: sends the browser to the provider, with the
/// cookie that binds the flow to it.
pub(crate) async fn login<B>(
    state: &AdminState,
    peer: Peer,
    request: &Request<B>,
) -> Response<Full<Bytes>> {
    let client = match offered(state, peer) {
        Ok(client) => client,
        Err(response) => return *response,
    };
    // Before anything is counted or kept: what another site made the
    // browser send would spend the sign-in attempts of the address and
    // replace the cookie of a flow under way.
    if is_sent_by_another_page(request) {
        return page(
            StatusCode::FORBIDDEN,
            "Not signed in",
            "The sign-in was refused: it has to be started from the dashboard, not from the \
             page of another site.",
            &client.dashboard,
            false,
        );
    }
    let client = match counted(state, client, peer).map(|()| client) {
        Ok(client) => client,
        Err(response) => return *response,
    };
    let start = match client.start().await {
        Ok(start) => start,
        Err(failure) => return refused(client, peer, &failure),
    };
    let Ok(location) = HeaderValue::from_str(&start.location) else {
        let failure = Failure::Provider(String::from(
            "the authorization endpoint of the provider is not an address a browser can go to",
        ));
        return refused(client, peer, &failure);
    };
    let mut response = Response::new(Full::new(Bytes::new()));
    *response.status_mut() = StatusCode::FOUND;
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, location);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let lifetime = FLOW_LIFETIME.as_secs();
    headers.insert(
        header::SET_COOKIE,
        binding_cookie(&start.binding, peer.tls, lifetime),
    );
    response
}

/// `GET /auth/oidc/callback`: where the provider sends the browser back,
/// with `query`; `binding` is the binding cookie of the request. Starts
/// the session of the user and answers with a page that goes on to the
/// dashboard, not with a redirect: the session cookie is `Strict`, and a
/// browser does not send it along a chain of redirects that began at the
/// provider.
pub(crate) async fn callback(
    state: &AdminState,
    peer: Peer,
    query: &str,
    binding: Option<&str>,
) -> Response<Full<Bytes>> {
    let client = match offered(state, peer) {
        Ok(client) => client,
        Err(response) => return *response,
    };
    let mut response = match client.finish(query, binding).await {
        Ok(identity) => {
            let Identity {
                name,
                roles,
                groups,
            } = identity;
            println!(
                "Admin => {name:?} signed in through the provider from {} with the roles \
                 {roles:?} of the groups {:?}",
                auth_api::client(peer),
                shown(&groups)
            );
            let id = state.sessions.create(&name, Some(roles), None);
            let mut response = page(
                StatusCode::OK,
                "Signed in",
                &format!("You are signed in as {name}."),
                &client.dashboard,
                true,
            );
            response
                .headers_mut()
                .append(header::SET_COOKIE, session::cookie(&id, peer.tls));
            response
        }
        Err(failure) => refused(client, peer, &failure),
    };
    response
        .headers_mut()
        .append(header::SET_COOKIE, binding_cookie("", peer.tls, 0));
    response
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const ISSUER: &str = "https://id.example.com/realms/ops";
    const NOW: u64 = 1_800_000_000;

    /// A compact JWT with `claims` as its payload.
    fn token(claims: &Value) -> String {
        let part = |text: String| URL_SAFE_NO_PAD.encode(text);
        format!(
            "{}.{}.{}",
            part(json!({ "alg": "RS256" }).to_string()),
            part(claims.to_string()),
            part(String::from("signature"))
        )
    }

    /// Claims that pass, with the members of `changed` over them (`null`
    /// removes one).
    fn claims(changed: Value) -> Value {
        let mut claims = json!({
            "iss": ISSUER,
            "aud": "rxh",
            "sub": "subject-1",
            "exp": NOW + 300,
            "nonce": "the-nonce",
        });
        for (name, value) in changed.as_object().unwrap() {
            if value.is_null() {
                claims.as_object_mut().unwrap().remove(name);
            } else {
                claims[name] = value.clone();
            }
        }
        claims
    }

    fn check(claims: &Value) -> Result<(), String> {
        check_claims(claims, ISSUER, "rxh", "the-nonce", NOW)
    }

    /// Settings for a provider that is not there.
    fn settings() -> config::Oidc {
        config::Oidc {
            issuer: String::from("http://127.0.0.1:1"),
            client_id: String::from("rxh"),
            client_secret: String::from("secret"),
            scopes: vec![String::from("openid")],
            groups_claim: String::from("groups"),
            name_claim: String::from("sub"),
            roles: BTreeMap::new(),
            ca: None,
        }
    }

    fn flow(binding: &str, created: Instant) -> Flow {
        Flow {
            binding: binding.to_owned(),
            nonce: String::from("nonce"),
            verifier: String::from("verifier"),
            created,
        }
    }

    #[test]
    fn the_payload_of_a_token_is_decoded() {
        let sent = json!({ "iss": ISSUER, "groups": ["ops"], "name": "Olga \u{1f600}" });
        assert_eq!(claims_of(&token(&sent)), Ok(sent));
    }

    #[test]
    fn a_token_with_two_or_four_parts_is_refused() {
        let good = token(&claims(json!({})));
        let (head, _signature) = good.rsplit_once('.').unwrap();
        assert!(claims_of(head).is_err(), "two parts");
        assert!(claims_of(&format!("{good}.more")).is_err(), "four parts");
        assert!(claims_of("").is_err());
        // Three parts whose payload is not base64url, not JSON, not an object.
        assert!(claims_of("a.b c.d").is_err());
        assert!(claims_of(&format!("a.{}.c", URL_SAFE_NO_PAD.encode("text"))).is_err());
        assert!(claims_of(&format!("a.{}.c", URL_SAFE_NO_PAD.encode("[1]"))).is_err());
        // The reason does not repeat the token.
        let reason = claims_of(&format!("{good}.more")).unwrap_err();
        assert!(!reason.contains(&good), "{reason}");
    }

    #[test]
    fn claims_pass_when_all_match() {
        assert_eq!(check(&claims(json!({}))), Ok(()));
        assert_eq!(check(&claims(json!({ "aud": ["rxh"] }))), Ok(()));
        assert_eq!(
            check(&claims(json!({ "aud": ["rxh", "other"], "azp": "rxh" }))),
            Ok(())
        );
        // The clock of the provider may be a minute behind.
        assert_eq!(check(&claims(json!({ "exp": NOW - 59 }))), Ok(()));
        assert_eq!(check(&claims(json!({ "exp": NOW as f64 + 0.5 }))), Ok(()));
        // An issuer is compared as it is written, a trailing `/` included.
        let slash = format!("{ISSUER}/");
        let claims = claims(json!({ "iss": slash }));
        assert_eq!(
            check_claims(&claims, &slash, "rxh", "the-nonce", NOW),
            Ok(())
        );
    }

    #[test]
    fn a_wrong_iss_is_refused() {
        for iss in [
            json!("https://id.example.com/realms/other"),
            json!(format!("{ISSUER}/")),
            json!(ISSUER.to_uppercase()),
            json!([ISSUER]),
            Value::Null,
        ] {
            let reason = check(&claims(json!({ "iss": iss }))).unwrap_err();
            assert!(reason.contains("issuer"), "{iss}: {reason}");
        }
    }

    #[test]
    fn a_wrong_aud_string_is_refused() {
        for aud in [
            json!("other"),
            json!("rxh2"),
            json!(""),
            json!(7),
            Value::Null,
        ] {
            let reason = check(&claims(json!({ "aud": aud }))).unwrap_err();
            assert!(reason.contains("audience"), "{aud}: {reason}");
        }
    }

    #[test]
    fn an_aud_list_without_the_client_is_refused() {
        for aud in [
            json!(["other"]),
            json!([]),
            json!(["other", "more"]),
            json!([["rxh"]]),
        ] {
            let reason = check(&claims(json!({ "aud": aud, "azp": "rxh" }))).unwrap_err();
            assert!(reason.contains("audience"), "{aud}: {reason}");
        }
    }

    #[test]
    fn an_aud_list_of_two_without_azp_is_refused() {
        let two = json!(["rxh", "other"]);
        let reason = check(&claims(json!({ "aud": two }))).unwrap_err();
        assert!(reason.contains("azp"), "{reason}");
        let reason = check(&claims(json!({ "aud": two, "azp": "other" }))).unwrap_err();
        assert!(reason.contains("azp"), "{reason}");
        // An `azp` of another client is wrong whatever the audience.
        let reason = check(&claims(json!({ "azp": "other" }))).unwrap_err();
        assert!(reason.contains("azp"), "{reason}");
    }

    #[test]
    fn a_past_exp_is_refused() {
        for exp in [json!(NOW - 60), json!(NOW - 3600), json!(0), json!(-5)] {
            let reason = check(&claims(json!({ "exp": exp }))).unwrap_err();
            assert!(reason.contains("expired"), "{exp}: {reason}");
        }
    }

    #[test]
    fn a_missing_exp_is_refused() {
        for exp in [
            Value::Null,
            json!("tomorrow"),
            json!(format!("{}", NOW + 300)),
        ] {
            let reason = check(&claims(json!({ "exp": exp }))).unwrap_err();
            assert!(reason.contains("exp"), "{exp}: {reason}");
        }
    }

    #[test]
    fn a_wrong_nonce_is_refused() {
        for nonce in [
            json!("another"),
            json!(""),
            json!(["the-nonce"]),
            Value::Null,
        ] {
            let reason = check(&claims(json!({ "nonce": nonce }))).unwrap_err();
            assert!(reason.contains("nonce"), "{nonce}: {reason}");
        }
    }

    #[test]
    fn the_name_falls_back_to_sub() {
        let named = json!({ "preferred_username": "olga", "sub": "subject-1" });
        assert_eq!(
            name_of(&named, "preferred_username").as_deref(),
            Some("olga")
        );
        assert_eq!(name_of(&named, "email").as_deref(), Some("subject-1"));
        for odd in [json!(""), json!(7), json!(["olga"]), Value::Null] {
            let claims = json!({ "preferred_username": odd, "sub": "subject-1" });
            assert_eq!(
                name_of(&claims, "preferred_username").as_deref(),
                Some("subject-1"),
                "{odd}"
            );
        }
        assert_eq!(name_of(&json!({ "sub": "" }), "preferred_username"), None);
        assert_eq!(name_of(&json!({}), "preferred_username"), None);
    }

    #[test]
    fn groups_are_a_string_or_a_list_of_strings() {
        let groups = |value: Value| groups_of(&json!({ "groups": value }), "groups");
        assert_eq!(groups(json!("ops")), ["ops"]);
        assert_eq!(groups(json!(["ops", "web"])), ["ops", "web"]);
        assert_eq!(
            groups(json!(["ops", 7, null, ["web"], "dev"])),
            ["ops", "dev"]
        );
        for none in [json!([]), json!(7), json!({ "ops": true }), Value::Null] {
            assert_eq!(groups(none.clone()), [] as [&str; 0], "{none}");
        }
        assert_eq!(
            groups_of(&json!({ "roles": ["ops"] }), "groups"),
            [] as [&str; 0]
        );
        assert_eq!(groups_of(&json!({ "roles": ["ops"] }), "roles"), ["ops"]);
    }

    #[test]
    fn roles_are_the_union_over_groups() {
        let list = |items: &[&str]| -> Vec<String> {
            items.iter().map(|item| (*item).to_owned()).collect()
        };
        let mapping: BTreeMap<String, Vec<String>> = [
            ("ops", list(&["web-editors", "admin"])),
            ("web", list(&["web-editors", "viewer"])),
            ("empty", list(&[])),
        ]
        .into_iter()
        .map(|(group, roles)| (group.to_owned(), roles))
        .collect();
        assert_eq!(
            roles_of(&list(&["web", "ops", "strangers", "ops"]), &mapping),
            ["admin", "viewer", "web-editors"]
        );
        assert_eq!(
            roles_of(&list(&["web"]), &mapping),
            ["viewer", "web-editors"]
        );
        // A group is its exact name.
        assert_eq!(
            roles_of(&list(&["OPS", "ops ", "strangers", "empty"]), &mapping),
            [] as [&str; 0]
        );
        assert_eq!(roles_of(&[], &mapping), [] as [&str; 0]);
    }

    #[test]
    fn the_challenge_is_s256_of_the_verifier() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn a_pending_flow_is_used_once_and_expires() {
        let start = Instant::now();
        let mut flows = HashMap::new();
        for state in ["one", "two", "three", "four", "old"] {
            remember(
                &mut flows,
                state.to_owned(),
                flow(&format!("of-{state}"), start),
            );
        }
        let soon = start + Duration::from_secs(1);

        let taken = take(&mut flows, "one", Some("of-one"), soon).expect("the flow");
        assert_eq!(
            (taken.nonce.as_str(), taken.verifier.as_str()),
            ("nonce", "verifier")
        );
        assert!(
            take(&mut flows, "one", Some("of-one"), soon).is_none(),
            "used"
        );

        // Without the cookie of its browser, or with another one, a state
        // gives nothing, and is spent.
        assert!(take(&mut flows, "two", None, soon).is_none());
        assert!(take(&mut flows, "two", Some("of-two"), soon).is_none());
        assert!(take(&mut flows, "three", Some("of-four"), soon).is_none());
        assert!(take(&mut flows, "three", Some("of-three"), soon).is_none());
        assert!(take(&mut flows, "four", Some(""), soon).is_none());
        assert!(take(&mut flows, "unknown", Some("of-one"), soon).is_none());

        let last = start + FLOW_LIFETIME - Duration::from_secs(1);
        remember(&mut flows, String::from("late"), flow("of-late", start));
        assert!(take(&mut flows, "late", Some("of-late"), last).is_some());
        assert!(take(&mut flows, "old", Some("of-old"), start + FLOW_LIFETIME).is_none());
        assert!(flows.is_empty());
    }

    #[test]
    fn the_oldest_pending_flow_goes_at_a_thousand() {
        let start = Instant::now();
        let second = Duration::from_secs(1);
        let mut flows = HashMap::new();
        // Each a tenth of a second after the one before.
        for index in 0..1000_u32 {
            let created = start + second * index / 10;
            remember(
                &mut flows,
                format!("state{index}"),
                flow("binding", created),
            );
        }
        assert_eq!(flows.len(), 1000, "a thousand fit");
        let now = start + second * 100;
        remember(&mut flows, String::from("newest"), flow("binding", now));
        assert_eq!(flows.len(), 1000);
        assert!(
            take(&mut flows, "state0", Some("binding"), now).is_none(),
            "the oldest went"
        );
        assert!(take(&mut flows, "state1", Some("binding"), now).is_some());
        assert!(take(&mut flows, "newest", Some("binding"), now).is_some());

        // The ones that are too old make room first.
        let later = start + FLOW_LIFETIME + second * 50;
        remember(&mut flows, String::from("later"), flow("binding", later));
        assert_eq!(flows.len(), 500);
    }

    #[test]
    fn text_is_escaped_for_html() {
        assert_eq!(
            escape("<a href=\"x\" onclick='y'>&</a>"),
            "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;&lt;/a&gt;"
        );
        assert_eq!(escape("olga@example.com, ops"), "olga@example.com, ops");
        // What is escaped is not escaped again by a second look at `&`.
        assert_eq!(escape("&lt;"), "&amp;lt;");
    }

    #[test]
    fn a_failure_has_its_status_and_names_few_groups() {
        let failure = Failure::NoRole {
            name: String::from("pat"),
            groups: vec![String::from("sales"), String::from("staff")],
        };
        let (status, sentence) = described(&failure);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            sentence.contains("pat") && sentence.contains("(sales, staff)"),
            "{sentence}"
        );
        let failure = Failure::Provider(String::from("it is down"));
        assert_eq!(described(&failure).0, StatusCode::BAD_GATEWAY);

        // Twenty groups at most, of 64 bytes at most, cut between characters.
        let many: Vec<String> = (0..25).map(|index| format!("group{index}")).collect();
        let text = shown(&many);
        assert!(text.ends_with("group19 and 5 more"), "{text}");
        assert!(!text.contains("group20"), "{text}");
        assert_eq!(shown(&many[..20]), many[..20].join(", "));
        let long = format!("{}\u{e9}{}", "a".repeat(63), "b".repeat(500));
        assert_eq!(shown(&[long]), format!("{}\u{2026}", "a".repeat(63)));
        assert_eq!(shown(&["b".repeat(64)]), "b".repeat(64));
        let huge: Vec<String> = (0..10_000).map(|_| "c".repeat(10_000)).collect();
        assert!(shown(&huge).len() < 2000);
    }

    #[test]
    fn a_name_with_control_characters_or_too_long_is_not_shown() {
        assert!(is_shown_as_it_is("olga@example.com"));
        assert!(is_shown_as_it_is("Olga Mart\u{ed}nez <ops>"));
        assert!(is_shown_as_it_is(&"a".repeat(256)));
        assert!(!is_shown_as_it_is(&"a".repeat(257)));
        for forged in [
            "eve\nAdmin => ana signed in",
            "eve\r",
            "e\u{1b}[2Jve",
            "eve\0",
            "eve\u{85}",
        ] {
            assert!(!is_shown_as_it_is(forged), "{forged:?}");
        }
    }

    #[test]
    fn a_page_has_its_policy() {
        let response = page(
            StatusCode::FORBIDDEN,
            "Title",
            "Text",
            "https://rxh.example.com/dashboard",
            false,
        );
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let headers = response.headers();
        assert_eq!(
            headers[header::CONTENT_SECURITY_POLICY],
            "default-src 'none'"
        );
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    }

    #[tokio::test]
    async fn the_flow_is_refused_where_no_session_may_be_held() {
        use std::net::{IpAddr, Ipv4Addr};
        use std::path::PathBuf;

        // Plain HTTP from another machine.
        let plain = Peer {
            local: false,
            tls: false,
            address: Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7))),
        };
        let request = Request::new(());
        let mut state = admin::tests::state(None, PathBuf::new());
        for response in [
            login(&state, plain, &request).await,
            callback(&state, plain, "code=a&state=b", Some("binding")).await,
        ] {
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "no [admin.oidc]");
        }

        // The provider is not there: an answer other than 403 would be 502.
        let client = Client::new(
            &settings(),
            "https://rxh.example.com",
            AdminGuard::default(),
        );
        state.oidc = Some(client.unwrap());
        for response in [
            login(&state, plain, &request).await,
            callback(&state, plain, "code=a&state=b", Some("binding")).await,
        ] {
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(!response.headers().contains_key(header::SET_COOKIE));
            assert!(!response.headers().contains_key(header::LOCATION));
        }
        assert!(state.oidc.as_ref().unwrap().pending().is_empty());
        // Where a session may be, the flow goes on, to the provider.
        let response = login(&state, admin::tests::LOCAL, &request).await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn a_ca_file_that_cannot_be_read_stops_the_client() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("no-such-ca.pem");
        // A directory is there and is not a file to read.
        for path in [missing.as_path(), directory.path()] {
            let settings = config::Oidc {
                ca: Some(path.to_str().unwrap().to_owned()),
                ..settings()
            };
            let Err(err) = Client::new(&settings, "https://rxh.example.com", AdminGuard::default())
            else {
                panic!("a client with the ca {}", path.display());
            };
            let message = err.to_string();
            assert!(message.starts_with("[admin.oidc]: "), "{message}");
            assert!(message.contains(path.to_str().unwrap()), "{message}");
        }
    }

    #[test]
    fn values_are_encoded_for_a_query() {
        assert_eq!(encode("openid profile"), "openid%20profile");
        assert_eq!(
            encode("https://rxh.example.com:2019/auth/oidc/callback"),
            "https%3A%2F%2Frxh.example.com%3A2019%2Fauth%2Foidc%2Fcallback"
        );
        assert_eq!(encode("a-b.c_d~e"), "a-b.c_d~e");
        assert_eq!(
            encode("s3cret/of+the=client&é"),
            "s3cret%2Fof%2Bthe%3Dclient%26%C3%A9"
        );
    }

    #[test]
    fn only_a_plain_error_code_is_repeated() {
        assert_eq!(named("access_denied"), " (access_denied)");
        for text in ["", "<b>no</b>", "two words", "line\nbreak", &"x".repeat(65)] {
            assert_eq!(named(text), "", "{text:?}");
        }
    }

    #[test]
    fn the_discovery_document_is_next_to_the_issuer() {
        assert_eq!(
            discovery_url("https://id.example.com"),
            "https://id.example.com/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url("https://tenant.example.com/"),
            "https://tenant.example.com/.well-known/openid-configuration"
        );
        assert_eq!(
            discovery_url("http://127.0.0.1:9000/realms/ops"),
            "http://127.0.0.1:9000/realms/ops/.well-known/openid-configuration"
        );
    }

    #[test]
    fn the_discovery_document_must_be_of_the_issuer_with_https_endpoints() {
        let document = |issuer: &str, authorization: &str, token: &str| {
            json!({
                "issuer": issuer,
                "authorization_endpoint": authorization,
                "token_endpoint": token,
            })
            .to_string()
        };
        let authorize = "https://id.example.com/authorize?tenant=ops";
        let token = "https://tokens.example.com/token";
        assert_eq!(
            endpoints_of(document(ISSUER, authorize, token).as_bytes(), ISSUER),
            Ok(Endpoints {
                authorization: authorize.to_owned(),
                token: token.to_owned(),
            })
        );
        let local = document(
            "http://127.0.0.1:9000",
            "http://127.0.0.1:9000/a",
            "http://[::1]:9000/t",
        );
        assert!(endpoints_of(local.as_bytes(), "http://127.0.0.1:9000").is_ok());

        // The issuer as it is written: a `/` more or less is another one.
        for issuer in [format!("{ISSUER}/"), String::from("https://evil.example")] {
            let reason =
                endpoints_of(document(&issuer, authorize, token).as_bytes(), ISSUER).unwrap_err();
            assert!(reason.contains("another issuer"), "{reason}");
        }
        for bad in [
            "http://id.example.com/token",
            "http://localhost/token",
            "https://user:password@id.example.com/token",
            "javascript:alert(1)",
            "/token",
            "",
        ] {
            let reason =
                endpoints_of(document(ISSUER, authorize, bad).as_bytes(), ISSUER).unwrap_err();
            assert!(reason.contains("token_endpoint"), "{bad}: {reason}");
            assert!(!reason.contains("password"), "{reason}");
            let reason = endpoints_of(document(ISSUER, bad, token).as_bytes(), ISSUER).unwrap_err();
            assert!(reason.contains("authorization_endpoint"), "{bad}: {reason}");
        }
        // What a provider on loopback may name, one over `https` may not:
        // the client secret would go in clear to a port of this machine.
        let mixed = document(ISSUER, authorize, "http://127.0.0.1:6379/token");
        let reason = endpoints_of(mixed.as_bytes(), ISSUER).unwrap_err();
        assert!(reason.contains("token_endpoint"), "{reason}");
        let mixed = document(ISSUER, "http://[::1]:9000/a", token);
        assert!(endpoints_of(mixed.as_bytes(), ISSUER).is_err());
        assert!(endpoints_of(b"<html>", ISSUER).is_err());
        assert!(endpoints_of(b"{}", ISSUER).is_err());
    }

    #[test]
    fn the_binding_cookie_is_lax_and_secure_over_tls() {
        assert_eq!(
            binding_cookie("abc", /* secure= */ false, 600),
            "rxh_oidc=abc; HttpOnly; SameSite=Lax; Path=/; Max-Age=600"
        );
        assert_eq!(
            binding_cookie("", /* secure= */ true, 0),
            "rxh_oidc=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0; Secure"
        );
    }
}
