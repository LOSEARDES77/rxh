//! `basic_auth` and `forward_auth`: who may send requests to a route.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use http_body_util::BodyExt;
use hyper::client::conn::http1;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{HeaderMap, Request, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use rxh_core::config::{self, UpstreamGroup, UpstreamTls};
use sha2::{Digest, Sha256};

use crate::admin_guard::AdminGuard;
use crate::http::body::{self, Guarded};
use crate::http::headers;
use crate::http::response::{self, BoxResponse};
use crate::stream::Dialer;

/// Bytes of a password that bcrypt looks at.
const BCRYPT_MAX: usize = 72;

/// Runtime state of one `basic_auth` table.
#[derive(Debug)]
pub(crate) struct BasicAuth {
    users: Arc<config::BasicAuth>,

    /// `WWW-Authenticate` of the `401` answers.
    challenge: HeaderValue,

    /// SHA-256 of the credentials that passed: bcrypt takes tens of
    /// milliseconds, and browsers send the credentials with every request.
    /// One entry per user at most, since only correct passwords get in.
    verified: Mutex<HashSet<[u8; 32]>>,
}

impl BasicAuth {
    pub(crate) fn new(settings: &config::BasicAuth) -> Self {
        let challenge = format!("Basic realm=\"{}\", charset=\"UTF-8\"", settings.realm);
        Self {
            users: Arc::new(settings.clone()),
            challenge: HeaderValue::from_str(&challenge)
                .expect("the configuration parser checked the realm"),
            verified: Mutex::default(),
        }
    }

    /// Returns true if `headers` carry the name and password of a user.
    pub(crate) async fn permits(&self, headers: &HeaderMap) -> bool {
        let credentials = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("basic"))
            .and_then(|(_, credentials)| config::BasicAuth::decode_credentials(credentials));
        let Some((user, password)) = credentials else {
            return false;
        };

        // Only what bcrypt compares, or a user could fill the set with
        // suffixes of a long password.
        let significant = &password[..password.len().min(BCRYPT_MAX)];
        let digest: [u8; 32] = Sha256::new()
            .chain_update(&user)
            .chain_update(b":")
            .chain_update(significant)
            .finalize()
            .into();
        if self.verified().contains(&digest) {
            return true;
        }

        let users = Arc::clone(&self.users);
        let verified = tokio::task::spawn_blocking(move || users.verify(&user, &password))
            .await
            .unwrap_or(false);
        if verified {
            self.verified().insert(digest);
        }
        verified
    }

    fn verified(&self) -> MutexGuard<'_, HashSet<[u8; 32]>> {
        self.verified
            .lock()
            .expect("nothing panics while holding the verified credentials")
    }

    /// `401 Unauthorized` asking for credentials.
    pub(crate) fn challenge(&self) -> BoxResponse {
        let mut response = response::plain(StatusCode::UNAUTHORIZED, "HTTP 401 UNAUTHORIZED");
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, self.challenge.clone());
        response
    }
}

/// Runtime state of one `forward_auth`.
#[derive(Debug)]
pub(crate) struct ForwardAuth {
    dialer: Dialer,

    /// `scheme://host[:port]` of the service, as the dialer takes it.
    address: String,

    /// `Host` of the requests to the service.
    host: HeaderValue,

    /// Path and query of the verification endpoint.
    target: Uri,

    copy_headers: Vec<HeaderName>,
    timeout: Duration,
}

impl ForwardAuth {
    /// Fails when the TLS settings for an `https://` service cannot be built.
    pub(crate) fn new(settings: &config::ForwardAuth, guard: AdminGuard) -> io::Result<Self> {
        let url: Uri = settings
            .url
            .parse()
            .expect("the configuration parser checked the url");
        let scheme = url.scheme_str().unwrap_or("http");
        let authority = url.authority().map_or("", |authority| authority.as_str());
        let group = UpstreamGroup {
            dial_timeout: settings.timeout,
            tls: (scheme == "https").then(UpstreamTls::default),
            ..UpstreamGroup::default()
        };
        let target = url.path_and_query().map_or("/", |path| path.as_str());
        Ok(Self {
            dialer: Dialer::new(&group, guard)?,
            address: format!("{scheme}://{authority}"),
            host: HeaderValue::from_str(authority).expect("a URI authority is visible ASCII"),
            target: target.parse().expect("part of a valid URI"),
            copy_headers: settings
                .copy_headers
                .iter()
                .map(|name| {
                    HeaderName::from_bytes(name.as_bytes())
                        .expect("the configuration parser checked the header names")
                })
                .collect(),
            timeout: settings.timeout,
        })
    }

    /// Sends `headers` (see [`headers::prepare_auth_request`]) to the service
    /// in a bodyless `GET`. On a `2xx` answer the request may go on: `None`,
    /// after replacing the `copy_headers` of `request_headers` with those of
    /// the answer. Any other answer is returned for the client, as is `502`
    /// or `504` when the service fails: no answer, no access.
    pub(crate) async fn check(
        &self,
        mut headers: HeaderMap,
        request_headers: &mut HeaderMap,
    ) -> Option<BoxResponse> {
        headers.insert(header::HOST, self.host.clone());
        let mut request = Request::new(body::empty());
        *request.uri_mut() = self.target.clone();
        *request.headers_mut() = headers;

        // shortcut: one connection per check, pool them when the latency of
        // the service shows up in profiles.
        let exchange = async {
            let stream = self.dialer.dial(&self.address).await?;
            let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
                .await
                .map_err(io::Error::other)?;
            tokio::task::spawn(connection);
            let response = sender
                .send_request(request)
                .await
                .map_err(io::Error::other)?;
            Ok::<_, io::Error>((response, sender))
        };
        let (response, sender) = match tokio::time::timeout(self.timeout, exchange).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => return Some(response::bad_gateway()),
            Err(_) => return Some(response::gateway_timeout()),
        };

        if response.status().is_success() {
            for name in &self.copy_headers {
                // Removed even when the service does not send it: a client
                // must not be able to set it.
                request_headers.remove(name);
                for value in response.headers().get_all(name) {
                    request_headers.append(name.clone(), value.clone());
                }
            }
            return None;
        }

        // The connection lives as long as the body is being read.
        let mut response =
            response.map(|incoming| Guarded::new(body::incoming(incoming), sender).boxed());
        headers::remove_hop_by_hop(response.headers_mut());
        Some(response)
    }
}
