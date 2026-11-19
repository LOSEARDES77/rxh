//! Health event hooks: a command and a webhook run when active health checks
//! mark a backend `healthy` or `unhealthy`.

use std::process::Stdio;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::{self, HeaderValue};
use hyper::{Request, Uri};
use hyper_util::rt::TokioIo;
use rxh_core::config::{HealthHooks, UpstreamGroup, UpstreamTls};
use serde::Serialize;
use tokio::process::Command;

use crate::stream::Dialer;

/// Limit for one command run or one webhook call.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// A backend changed state. Serialized as the webhook body.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    /// `healthy` or `unhealthy`.
    pub(crate) event: &'static str,

    /// Pool name.
    pub(crate) pool: String,

    /// Backend address.
    pub(crate) upstream: String,

    /// Why the last check failed, for `unhealthy`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// The hooks of one pool, ready to run.
#[derive(Debug)]
pub(crate) struct Hooks {
    command: Vec<String>,
    webhook: Option<Webhook>,
}

/// An `http://` or `https://` URL that JSON documents are posted to.
#[derive(Debug)]
pub(crate) struct Webhook {
    uri: Uri,
    address: String,
    dialer: Dialer,
}

impl Hooks {
    /// Prepares `settings`, which passed configuration validation.
    pub(crate) fn new(settings: &HealthHooks) -> Self {
        let webhook = settings.webhook.as_deref().map(Webhook::new);
        Self {
            command: settings.command.clone(),
            webhook,
        }
    }

    /// Runs the command, then calls the webhook. Failures are logged.
    pub(crate) async fn run(&self, event: &Event) {
        if !self.command.is_empty()
            && let Err(err) = tokio::time::timeout(HOOK_TIMEOUT, self.run_command(event))
                .await
                .unwrap_or_else(|_| Err(format!("no exit within {HOOK_TIMEOUT:?}")))
        {
            eprintln!("{} => health hook command failed: {err}", event.pool);
        }
        if let Some(webhook) = &self.webhook
            && let Err(err) = webhook
                .post(
                    serde_json::to_vec(event).expect("events serialize to JSON"),
                    "rxh-health-hook",
                )
                .await
        {
            eprintln!(
                "{} => health hook webhook {} failed: {err}",
                event.pool, webhook.uri
            );
        }
    }

    async fn run_command(&self, event: &Event) -> Result<(), String> {
        let (program, args) = self
            .command
            .split_first()
            .expect("run_command is only called with a command");
        // kill_on_drop stops the child when the timeout drops this future.
        let status = Command::new(program)
            .args(args)
            .env("RXH_EVENT", event.event)
            .env("RXH_POOL", &event.pool)
            .env("RXH_UPSTREAM", &event.upstream)
            .env("RXH_REASON", event.reason.as_deref().unwrap_or_default())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .map_err(|err| format!("{program}: {err}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{program}: {status}"))
        }
    }
}

impl Webhook {
    /// Prepares `url`, which passed configuration validation.
    pub(crate) fn new(url: &str) -> Self {
        let uri: Uri = url.parse().expect("validated URL");
        let https = uri.scheme_str() == Some("https");
        let host = uri.host().expect("validated URL has a host");
        let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
        let group = UpstreamGroup {
            tls: https.then(UpstreamTls::default),
            dial_timeout: HOOK_TIMEOUT,
            ..UpstreamGroup::default()
        };
        Self {
            address: format!("{host}:{port}"),
            dialer: Dialer::new(&group).expect("default TLS settings read no files"),
            uri,
        }
    }

    pub(crate) fn uri(&self) -> &Uri {
        &self.uri
    }

    /// Posts the JSON document `body`. Anything but a 2xx answer within
    /// [`HOOK_TIMEOUT`] is an error.
    pub(crate) async fn post(&self, body: Vec<u8>, user_agent: &'static str) -> Result<(), String> {
        tokio::time::timeout(HOOK_TIMEOUT, self.send(body, user_agent))
            .await
            .unwrap_or_else(|_| Err(format!("no answer within {HOOK_TIMEOUT:?}")))
    }

    async fn send(&self, body: Vec<u8>, user_agent: &'static str) -> Result<(), String> {
        let stream = self
            .dialer
            .dial(&self.address)
            .await
            .map_err(|err| err.to_string())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|err| err.to_string())?;
        tokio::task::spawn(connection);

        let path = self.uri.path_and_query().map_or("/", |path| path.as_str());
        let host = self
            .uri
            .authority()
            .expect("validated URL has a host")
            .as_str();
        let request = Request::post(path)
            .header(header::HOST, host)
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .header(header::USER_AGENT, HeaderValue::from_static(user_agent))
            .body(Full::new(Bytes::from(body)))
            .map_err(|err| err.to_string())?;
        let status = sender
            .send_request(request)
            .await
            .map_err(|err| err.to_string())?
            .status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("status {status}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_body_names_the_event() {
        let event = Event {
            event: "unhealthy",
            pool: String::from("web"),
            upstream: String::from("127.0.0.1:8080"),
            reason: Some(String::from("connection refused")),
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"event":"unhealthy","pool":"web","upstream":"127.0.0.1:8080","reason":"connection refused"}"#
        );
        let healthy = Event {
            event: "healthy",
            reason: None,
            ..event
        };
        assert!(!serde_json::to_string(&healthy).unwrap().contains("reason"));
    }
}
