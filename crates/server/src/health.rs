//! Active health checks. With `health.grpc` the check is a call of the gRPC
//! health checking protocol, with `health.uri` an HTTP request, otherwise a
//! TCP connect.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::Method;
use hyper::Request;
use hyper::Uri;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rxh_core::address::BackendAddress;
use rxh_core::config::ActiveHealth;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

use crate::hooks::{Event, Hooks};
use crate::registry::Group;

const MAX_REDIRECTS: usize = 5;
const MAX_BODY: usize = 1024 * 1024;

/// Checks every member of `group` forever, at `health.interval`.
pub(crate) async fn run(group: Arc<Group>) {
    let Some(settings) = group.pool.settings().health.clone() else {
        return;
    };
    let settings = Arc::new(settings);

    // One worker per pool keeps hook runs in event order without delaying checks.
    let hooks = settings.hooks.as_ref().map(|hooks| {
        let hooks = Hooks::new(hooks);
        let (sender, mut receiver) = mpsc::unbounded_channel::<Event>();
        tokio::task::spawn(async move {
            while let Some(event) = receiver.recv().await {
                hooks.run(&event).await;
            }
        });
        sender
    });

    let mut interval = tokio::time::interval(settings.interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        interval.tick().await;

        let mut checks = JoinSet::new();
        for upstream in group.pool.upstreams() {
            let group = Arc::clone(&group);
            let settings = Arc::clone(&settings);
            checks.spawn(async move {
                let result = check(&group, &settings, upstream.address()).await;
                (upstream, result)
            });
        }

        while let Some(Ok((upstream, result))) = checks.join_next().await {
            let change = upstream.record_check(result.is_ok(), settings.passes, settings.fails);
            let (kind, reason) = match (change, result) {
                (Some(true), _) => {
                    println!("{} => {} is healthy", group.pool.name(), upstream.address());
                    ("healthy", None)
                }
                (Some(false), Err(reason)) => {
                    eprintln!(
                        "{} => {} is unhealthy: {reason}",
                        group.pool.name(),
                        upstream.address()
                    );
                    ("unhealthy", Some(reason))
                }
                _ => continue,
            };
            if let Some(hooks) = &hooks {
                let _ = hooks.send(Event {
                    event: kind,
                    pool: group.pool.name().to_owned(),
                    upstream: upstream.address().to_owned(),
                    reason,
                });
            }
        }
    }
}

/// Runs one check against `address`.
async fn check(group: &Group, settings: &ActiveHealth, address: &str) -> Result<(), String> {
    let target = match (&settings.upstream, settings.port) {
        (Some(upstream), _) => upstream.clone(),
        (None, Some(port)) => match BackendAddress::parse(address) {
            Ok(BackendAddress::Tcp { host, .. }) => format!("{host}:{port}"),
            _ => address.to_owned(),
        },
        (None, None) => address.to_owned(),
    };

    let attempt = async {
        match &settings.uri {
            _ if settings.grpc => grpc_check(group, settings, &target).await,
            None => group
                .dialer
                .dial(&target)
                .await
                .map(drop)
                .map_err(|err| err.to_string()),
            Some(uri) => http_check(group, settings, &target, uri).await,
        }
    };

    tokio::time::timeout(settings.timeout, attempt)
        .await
        .map_err(|_| format!("no answer within {:?}", settings.timeout))?
}

async fn http_check(
    group: &Group,
    settings: &ActiveHealth,
    target: &str,
    uri: &str,
) -> Result<(), String> {
    let method = Method::from_bytes(settings.method.as_bytes()).map_err(|err| err.to_string())?;
    let mut path = uri.to_owned();
    let host_header = match BackendAddress::parse(target) {
        Ok(address) => address
            .host_port()
            .unwrap_or_else(|| String::from("localhost")),
        Err(_) => target.to_owned(),
    };

    for _ in 0..=MAX_REDIRECTS {
        let stream = group
            .dialer
            .dial(target)
            .await
            .map_err(|err| err.to_string())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| err.to_string())?;
        tokio::task::spawn(connection);

        let body = settings.request_body.clone().unwrap_or_default();
        let mut request = Request::new(Full::new(Bytes::from(body)));
        *request.method_mut() = method.clone();
        *request.uri_mut() = path
            .parse()
            .map_err(|_| format!("invalid health uri {path:?}"))?;
        let headers = request.headers_mut();
        headers.insert(
            header::HOST,
            HeaderValue::from_str(&host_header).map_err(|err| err.to_string())?,
        );
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("rxh-health-check"),
        );
        for (name, value) in &settings.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| err.to_string())?;
            let value = HeaderValue::from_str(value).map_err(|err| err.to_string())?;
            headers.insert(name, value);
        }

        let response = sender
            .send_request(request)
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status();

        if status.is_redirection() && settings.follow_redirects {
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<Uri>().ok())
                .and_then(|uri| uri.path_and_query().map(|path| path.as_str().to_owned()));
            match location {
                Some(location) => {
                    path = location;
                    continue;
                }
                None => return Err(format!("redirect {status} without a usable Location")),
            }
        }

        let expected = settings.status.unwrap_or_default();
        if !expected.matches(status.as_u16()) {
            return Err(format!("status {status}, expected {expected}"));
        }

        if let Some(pattern) = &group.health_body {
            let body = Limited::new(response.into_body(), MAX_BODY)
                .collect()
                .await
                .map_err(|err| err.to_string())?
                .to_bytes();
            if !pattern.is_match(&String::from_utf8_lossy(&body)) {
                return Err(format!("body does not match {:?}", pattern.as_str()));
            }
        }

        return Ok(());
    }

    Err(String::from("too many redirects"))
}

/// Calls `grpc.health.v1.Health/Check` over HTTP/2 and passes on `SERVING`.
async fn grpc_check(group: &Group, settings: &ActiveHealth, target: &str) -> Result<(), String> {
    let authority = match BackendAddress::parse(target) {
        Ok(address) => address
            .host_port()
            .unwrap_or_else(|| String::from("localhost")),
        Err(_) => target.to_owned(),
    };
    let stream = group
        .dialer
        .dial(target)
        .await
        .map_err(|err| err.to_string())?;
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
            .await
            .map_err(|err| err.to_string())?;
    tokio::task::spawn(connection);

    let service = settings.grpc_service.as_deref().unwrap_or_default();
    let mut request = Request::new(Full::new(Bytes::from(grpc_health_request(service))));
    *request.method_mut() = Method::POST;
    *request.uri_mut() = format!("http://{authority}/grpc.health.v1.Health/Check")
        .parse()
        .map_err(|_| format!("invalid health target {target:?}"))?;
    let headers = request.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc"),
    );
    headers.insert(header::TE, HeaderValue::from_static("trailers"));
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("rxh-health-check"),
    );
    for (name, value) in &settings.headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|err| err.to_string())?;
        let value = HeaderValue::from_str(value).map_err(|err| err.to_string())?;
        headers.insert(name, value);
    }

    let response = sender
        .send_request(request)
        .await
        .map_err(|err| err.to_string())?;
    let (parts, body) = response.into_parts();
    if !parts.status.is_success() {
        return Err(format!("status {}, expected 200", parts.status));
    }
    let collected = Limited::new(body, MAX_BODY)
        .collect()
        .await
        .map_err(|err| err.to_string())?;
    // An error without a message comes as headers only.
    let grpc_status = collected
        .trailers()
        .and_then(|trailers| trailers.get("grpc-status"))
        .or_else(|| parts.headers.get("grpc-status"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing");
    if grpc_status != "0" {
        return Err(format!("grpc-status {grpc_status}, expected 0"));
    }
    grpc_health_serving(&collected.to_bytes())
}

/// Length-prefixed `HealthCheckRequest` for `service`.
fn grpc_health_request(service: &str) -> Vec<u8> {
    let mut message = Vec::new();
    if !service.is_empty() {
        // Field 1, length-delimited, with a varint length.
        message.push(0x0a);
        let mut len = service.len();
        while len >= 0x80 {
            message.push(len as u8 | 0x80);
            len >>= 7;
        }
        message.push(len as u8);
        message.extend_from_slice(service.as_bytes());
    }
    let mut frame = vec![0];
    frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
    frame.extend_from_slice(&message);
    frame
}

/// Checks that `body` is one uncompressed `HealthCheckResponse` with status
/// `SERVING`.
fn grpc_health_serving(body: &[u8]) -> Result<(), String> {
    // The message has a single field: `status` (field 1, varint), omitted
    // when it is `UNKNOWN` (0).
    match body {
        [0, 0, 0, 0, 2, 0x08, 1] => Ok(()),
        [0, 0, 0, 0, 0] | [0, 0, 0, 0, 2, 0x08, 0] => Err(String::from("status UNKNOWN")),
        [0, 0, 0, 0, 2, 0x08, 2] => Err(String::from("status NOT_SERVING")),
        [0, 0, 0, 0, 2, 0x08, 3] => Err(String::from("status SERVICE_UNKNOWN")),
        _ => Err(String::from("unreadable HealthCheckResponse")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_health_request_frames_the_service_name() {
        assert_eq!(grpc_health_request(""), [0, 0, 0, 0, 0]);
        assert_eq!(
            grpc_health_request("a.B"),
            [0, 0, 0, 0, 5, 0x0a, 3, b'a', b'.', b'B']
        );
        let long = "x".repeat(300);
        let frame = grpc_health_request(&long);
        assert_eq!(frame[..5], [0, 0, 0, 1, 47]);
        // 300 as a varint.
        assert_eq!(frame[5..8], [0x0a, 0xac, 0x02]);
        assert_eq!(frame.len(), 5 + 3 + 300);
    }

    #[test]
    fn grpc_health_response_must_be_serving() {
        assert_eq!(grpc_health_serving(&[0, 0, 0, 0, 2, 8, 1]), Ok(()));
        let failure = |body: &[u8]| grpc_health_serving(body).unwrap_err();
        assert_eq!(failure(&[0, 0, 0, 0, 2, 8, 2]), "status NOT_SERVING");
        assert_eq!(failure(&[0, 0, 0, 0, 2, 8, 3]), "status SERVICE_UNKNOWN");
        assert_eq!(failure(&[0, 0, 0, 0, 0]), "status UNKNOWN");
        assert_eq!(failure(&[]), "unreadable HealthCheckResponse");
        // Compressed messages are not requested, so not understood.
        assert_eq!(
            failure(&[1, 0, 0, 0, 2, 8, 1]),
            "unreadable HealthCheckResponse"
        );
    }
}
