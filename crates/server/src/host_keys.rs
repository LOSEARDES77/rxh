//! The keys of a `[[server]]` table that a caller of the admin API without
//! the `admin` role may set, and the check of a change against them.
//!
//! A host table can make the daemon read files, run programs, call other
//! services and open sockets, so Modify and Create do not mean "any table".
//! [`ALLOWED`] has the keys that route or answer traffic for the host and do
//! nothing else. Every other key, known ([`DENIED`]) or not, keeps the value
//! it has in the file, or stays absent.
//!
//! A key is named by its path in the table without the positions in arrays,
//! and without the `match` and `subroute` it is under: an entry has the keys
//! of a server. A chain of `fallback` counts as one. `**` stands for
//! everything below a key.

use serde_json::Value as Json;

/// What a caller without the `admin` role may set.
pub(crate) const ALLOWED: &[&str] = &[
    // What the grants are about, checked with them.
    "name",
    "groups",
    // Checked against the running listeners by the caller of [`check`].
    "listen",
    // Which requests the host takes.
    "host",
    "http_host",
    "path",
    "uri",
    "method",
    "query.**",
    "header.**",
    "regexp.**",
    "regexp_count",
    "remote_ip",
    "sni",
    "alpn",
    "ip_allow",
    "ip_deny",
    "rate_limit.**",
    // What it answers.
    "forward",
    "redirect",
    "redirect.**",
    "rewrite",
    "request.**",
    "response.**",
    "cors.**",
    "encode",
    "security_headers",
    "cache.enabled",
    "cache.ttl_seconds",
    "handle_response.status",
    "handle_response.header.**",
    "handle_response.redirect",
    "handle_response.redirect.**",
    "handle_response.replace_status",
    "handle_response.rewrite",
    "handle_response.copy_response",
    // `"auto"` only: the files of a certificate are `tls.cert` and `tls.key`.
    "tls",
    // An inline pool: where it sends the requests of the host, and how.
    "forward.backends",
    "forward.address",
    "forward.backends.address",
    "forward.backends.weight",
    "forward.pool",
    "forward.weight",
    "forward.policy",
    "forward.choose",
    "forward.key",
    "forward.hash_type",
    "forward.hash_balance_factor",
    "forward.retries",
    "forward.try_duration",
    "forward.try_interval",
    "forward.retry_budget.**",
    "forward.retry_match.**",
    "forward.hedge_delay",
    "forward.dial_timeout",
    "forward.read_timeout",
    "forward.write_timeout",
    "forward.response_header_timeout",
    "forward.queue_timeout",
    "forward.flush_interval",
    "forward.h2c",
    "forward.grpc_web",
    "forward.slow_start",
    "forward.passive.**",
    "forward.circuit_breaker.**",
    "forward.health.interval",
    "forward.health.timeout",
    "forward.health.uri",
    "forward.health.method",
    "forward.health.port",
    "forward.health.headers.**",
    "forward.health.request_body",
    "forward.health.status",
    "forward.health.body",
    "forward.health.passes",
    "forward.health.fails",
    "forward.health.grpc",
    "forward.health.grpc_service",
    "forward.tls.server_name",
    "forward.tls.insecure_skip_verify",
    "forward.fallback",
    "forward.fallback.policy",
    "forward.fallback.choose",
    "forward.fallback.key",
];

/// What only an administrator sets, each for the reason given. A key that
/// is on neither list is treated like these; the test below fails until it
/// is on one.
// Read by the test only: the check refuses whatever `ALLOWED` lacks.
#[cfg_attr(not(test), expect(dead_code))]
pub(crate) const DENIED: &[&str] = &[
    // Paths on the machine of the daemon.
    "serve.**",
    "handle_response.serve.**",
    "error_pages.**",
    "tls.cert",
    "tls.key",
    "forward.tls.ca",
    "forward.tls.client_cert",
    "forward.tls.client_key",
    // Programs, and requests to somewhere else than the forward targets.
    "forward.health.hooks.**",
    "forward.health.upstream",
    "forward.health.follow_redirects",
    "forward.dynamic.**",
    "forward_auth.**",
    "mirror.**",
    // Credentials.
    "basic_auth.**",
    "forward.secret",
    "forward.fallback.secret",
    // Who is trusted.
    "trusted_proxies",
    "client_auth.**",
    "proxy_protocol",
    "proxy_protocol_in",
    // Sockets and what every server of a shared listener gets.
    "protocol",
    "http2",
    "http3",
    "https_redirect",
    "tls_options.**",
    "max_connections",
    "max_connections_per_ip",
    "match_timeout",
    "idle_timeout",
    "read_header_timeout",
    "stream_timeout",
    "stream_close_delay",
    "udp_idle_timeout",
    // Memory and connections of the daemon.
    "cache.max_body_bytes",
    "cache.max_entries",
    "forward.keepalive",
    "forward.keepalive_idle_conns_per_host",
    "forward.http2_conns_per_host",
    "forward.max_conns_per_host",
    "forward.max_connections",
    "forward.queue_size",
    "forward.request_buffers",
    "forward.response_buffers",
];

/// Whether `key` is in `list`, by itself or below a `**` entry.
fn lists(list: &[&str], key: &str) -> bool {
    list.iter().any(|entry| match entry.strip_suffix(".**") {
        Some(parent) => key
            .strip_prefix(parent)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.')),
        None => *entry == key,
    })
}

/// Whether an entry of [`ALLOWED`] is `key` or below it: an empty table at
/// `key` may come or go.
fn allows_below(key: &str) -> bool {
    ALLOWED.iter().any(|entry| {
        entry
            .strip_prefix(key)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    })
}

/// One step into a table: a key, or a position in an array.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step<'a> {
    Key(&'a str),
    Index(usize),
}

/// The steps into `value`, none for a scalar.
fn steps(value: &Json) -> Option<Vec<Step<'_>>> {
    match value {
        Json::Object(object) => Some(object.keys().map(|key| Step::Key(key)).collect()),
        Json::Array(array) => Some((0..array.len()).map(Step::Index).collect()),
        _ => None,
    }
}

fn get<'a>(value: Option<&'a Json>, step: Step<'_>) -> Option<&'a Json> {
    match step {
        Step::Key(key) => value?.as_object()?.get(key),
        Step::Index(index) => value?.as_array()?.get(index),
    }
}

/// The name in the lists of what is one `step` below `key`.
fn key_below(key: &str, step: Step<'_>) -> String {
    match step {
        Step::Index(_) => key.to_owned(),
        // The entries of a server have the keys of a server.
        Step::Key("match" | "subroute") if key.is_empty() => String::new(),
        Step::Key("fallback") if key.ends_with(".fallback") => key.to_owned(),
        Step::Key(name) if key.is_empty() => name.to_owned(),
        Step::Key(name) => format!("{key}.{name}"),
    }
}

/// The path shown to the caller of what is one `step` below `path`.
fn path_below(path: &str, step: Step<'_>) -> String {
    match step {
        Step::Index(index) => format!("{path}[{index}]"),
        Step::Key(name) if path.is_empty() => name.to_owned(),
        Step::Key(name) => format!("{path}.{name}"),
    }
}

/// Whether a caller without the `admin` role may turn the table `before`
/// of a host (`None` for a host it creates) into `after`: what differs is
/// on [`ALLOWED`], no string it sets has an `{env.NAME}` or `{file.PATH}`
/// placeholder, and no forward target it sets is a unix socket. The error
/// is the answer to the caller, and names the first key that may not change.
///
/// Entries of an array are compared by position: one that moves counts as
/// changed.
pub(crate) fn check(before: Option<&Json>, after: &Json) -> Result<(), String> {
    compare(before, Some(after), "", "")
}

fn compare(
    before: Option<&Json>,
    after: Option<&Json>,
    path: &str,
    key: &str,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    let old: Option<Vec<Step<'_>>> = before.and_then(steps);
    let new: Option<Vec<Step<'_>>> = after.and_then(steps);
    let is_scalar =
        |value: Option<&Json>, steps: &Option<Vec<Step<'_>>>| value.is_some() && steps.is_none();
    if is_scalar(before, &old) || is_scalar(after, &new) || (old.is_none() && new.is_none()) {
        set(after, path, key)?;
    }
    // A table or an array that comes or goes as a whole, even empty.
    let whole = !key.is_empty() && (before.is_none() || after.is_none());
    if whole && !allows_below(key) && !lists(ALLOWED, key) {
        return Err(needs_admin(path));
    }
    let mut all: Vec<Step<'_>> = old.unwrap_or_default();
    for step in new.unwrap_or_default() {
        if !all.contains(&step) {
            all.push(step);
        }
    }
    for step in all {
        compare(
            get(before, step),
            get(after, step),
            &path_below(path, step),
            &key_below(key, step),
        )?;
    }
    Ok(())
}

/// Whether the scalar at `key` may become `after`, or go.
fn set(after: Option<&Json>, path: &str, key: &str) -> Result<(), String> {
    if !lists(ALLOWED, key) {
        return Err(needs_admin(path));
    }
    let Some(Json::String(text)) = after else {
        return Ok(());
    };
    if text.contains("{env.") || text.contains("{file.") {
        return Err(format!(
            "a value read from the environment or a file needs the admin role: {path}"
        ));
    }
    let is_target = ["forward", "backends", "address"]
        .iter()
        .any(|name| key == *name || key.ends_with(&format!(".{name}")));
    if is_target && text.starts_with("unix/") {
        return Err(format!(
            "a unix socket as a forward target needs the admin role: {path}"
        ));
    }
    Ok(())
}

fn needs_admin(path: &str) -> String {
    format!("this needs the admin role: {path}")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rxh_core::config::Config;
    use serde_json::json;

    use super::*;

    /// The names in the lists of every key the schema node `node` can hold.
    fn keys_of(
        schema: &Json,
        node: &Json,
        key: &str,
        seen: &mut BTreeSet<(String, String)>,
        keys: &mut BTreeSet<String>,
    ) {
        if let Some(reference) = node.get("$ref").and_then(Json::as_str) {
            let name = reference.rsplit('/').next().unwrap();
            if seen.insert((name.to_owned(), key.to_owned())) {
                keys_of(schema, &schema["definitions"][name], key, seen, keys);
            }
            return;
        }
        let mut is_scalar = true;
        for alternatives in ["allOf", "anyOf", "oneOf"] {
            for alternative in node[alternatives].as_array().into_iter().flatten() {
                is_scalar = false;
                keys_of(schema, alternative, key, seen, keys);
            }
        }
        for (name, property) in node["properties"].as_object().into_iter().flatten() {
            is_scalar = false;
            let below = key_below(key, Step::Key(name));
            keys_of(schema, property, &below, seen, keys);
        }
        if node["additionalProperties"].is_object() {
            is_scalar = false;
            let below = format!("{key}.*");
            keys_of(schema, &node["additionalProperties"], &below, seen, keys);
        }
        if let Some(items) = node.get("items") {
            is_scalar = false;
            keys_of(schema, items, key, seen, keys);
        }
        // `null` next to a table says the table is optional.
        if is_scalar && !key.is_empty() && node["type"] != "null" {
            keys.insert(key.to_owned());
        }
    }

    #[test]
    fn every_key_of_a_server_is_allowed_or_denied() {
        let schema = serde_json::to_value(Config::json_schema()).unwrap();
        let mut keys = BTreeSet::new();
        let server = &schema["definitions"]["Server"];
        keys_of(&schema, server, "", &mut BTreeSet::new(), &mut keys);
        assert!(keys.contains("forward.health.hooks.command"), "{keys:?}");
        assert!(keys.contains("request.headers.set.*"), "{keys:?}");
        assert!(keys.len() > 150, "the walk reaches the model: {keys:?}");

        let undecided: Vec<&String> = keys
            .iter()
            .filter(|key| lists(ALLOWED, key) == lists(DENIED, key))
            .collect();
        assert!(
            undecided.is_empty(),
            "decide whether a user without the admin role may set these keys of a host, and \
             add each to ALLOWED or to DENIED: {undecided:?}"
        );
        // An entry that names no key of the model is a typo.
        for entry in ALLOWED.iter().chain(DENIED) {
            let parent = entry.strip_suffix(".**").unwrap_or(entry);
            assert!(
                keys.iter().any(|key| lists(&[entry], key)) || keys.contains(parent),
                "{entry} names no key of the schema"
            );
        }
    }

    #[test]
    fn what_differs_must_be_allowed() {
        let stored = json!({
            "name": "shop",
            "listen": "127.0.0.1:80",
            "serve": "/srv/shop",
            "match": [
                { "uri": "/a", "forward": "@api" },
                { "uri": "/b", "forward": { "backends": ["10.0.0.1:80"], "secret": "s" } },
            ],
        });
        let with = |change: &dyn Fn(&mut Json)| {
            let mut after = stored.clone();
            change(&mut after);
            check(Some(&stored), &after)
        };
        let refused = |change: &dyn Fn(&mut Json), path: &str| {
            assert_eq!(
                with(change),
                Err(format!("this needs the admin role: {path}"))
            );
        };

        assert_eq!(with(&|_| {}), Ok(()));
        assert_eq!(
            with(&|after| after["match"][0]["uri"] = json!("/c")),
            Ok(())
        );
        assert_eq!(
            with(&|after| after["match"][1]["forward"]["backends"] = json!(["10.0.0.2:80"])),
            Ok(())
        );
        assert_eq!(
            with(&|after| after["request"] = json!({ "headers": { "set": { "X-A": "b" } } })),
            Ok(())
        );
        assert_eq!(with(&|after| after["tls"] = json!("auto")), Ok(()));

        refused(&|after| after["serve"] = json!("/"), "serve");
        refused(
            &|after| {
                after.as_object_mut().unwrap().remove("serve");
            },
            "serve",
        );
        refused(
            &|after| after["serve"] = json!({ "root": "/srv/shop" }),
            "serve",
        );
        refused(
            &|after| after["match"][1]["forward"]["secret"] = json!("t"),
            "match[1].forward.secret",
        );
        refused(
            &|after| after["match"][1]["forward"] = json!("@api"),
            "match[1].forward.secret",
        );
        // An entry with a key for administrators does not move.
        refused(
            &|after| {
                after["match"].as_array_mut().unwrap().remove(0);
            },
            "match[0].forward.secret",
        );
        refused(
            &|after| {
                after["match"][0]["forward"] = json!({ "backends": ["10.0.0.1:80"], "health": { "hooks": { "command": ["id"] } } });
            },
            "match[0].forward.health.hooks",
        );
        refused(
            &|after| after["match"][0]["subroute"] = json!([{ "serve": "/" }]),
            "match[0].subroute[0].serve",
        );
        refused(
            &|after| after["tls"] = json!({ "cert": "/a", "key": "/b" }),
            "tls.cert",
        );
        refused(
            &|after| after["basic_auth"] = json!({ "users": {} }),
            "basic_auth",
        );
        refused(&|after| after["error_pages"] = json!({}), "error_pages");
        refused(&|after| after["brand_new_key"] = json!(1), "brand_new_key");
        refused(&|after| after["match"] = json!("x"), "match");
        refused(
            &|after| {
                after["match"][0]["forward"] =
                    json!({ "fallback": { "fallback": { "secret": "x" } } })
            },
            "match[0].forward.fallback.fallback.secret",
        );
        assert_eq!(
            with(
                &|after| after["match"][0]["forward"] = json!({ "backends": ["10.0.0.1:80"], "fallback": { "fallback": { "policy": "random" } } })
            ),
            Ok(())
        );
    }

    #[test]
    fn a_new_host_has_allowed_keys_only() {
        let host = json!({ "listen": "127.0.0.1:80", "host": ["a.example"], "forward": "@api" });
        assert_eq!(check(None, &host), Ok(()));
        let mut served = host.clone();
        served["serve"] = json!("/");
        assert_eq!(
            check(None, &served),
            Err(String::from("this needs the admin role: serve"))
        );
    }

    #[test]
    fn placeholders_and_unix_targets_are_not_set() {
        let stored = json!({
            "forward": "unix//run/app.sock",
            "response": { "headers": { "set": { "X-Region": "{env.REGION}" } } },
        });
        // What an administrator wrote stays, next to a change.
        let mut after = stored.clone();
        after["rewrite"] = json!("/v2{uri}");
        assert_eq!(check(Some(&stored), &after), Ok(()));

        let set = |path: &[&str], value: Json| {
            let mut after = json!({ "forward": "@api" });
            let mut at = &mut after;
            for step in path {
                at = &mut at[*step];
            }
            *at = value;
            check(Some(&json!({ "forward": "@api" })), &after).unwrap_err()
        };
        for placeholder in ["{env.HOME}", "a{file./etc/passwd}"] {
            let error = set(&["response", "headers", "set", "X-A"], json!(placeholder));
            assert!(error.contains("environment or a file"), "{error}");
            assert!(error.ends_with(": response.headers.set.X-A"), "{error}");
            let error = set(&["host"], json!(["a.example", placeholder]));
            assert!(error.ends_with(": host[1]"), "{error}");
        }
        for (path, value) in [
            (["forward"].as_slice(), json!("unix//run/admin.sock")),
            (&["forward"], json!(["10.0.0.1:80", "unix//run/admin.sock"])),
            (
                &["forward"],
                json!({ "backends": ["unix//run/admin.sock"] }),
            ),
            (
                &["forward"],
                json!({ "backends": [{ "address": "unix//run/admin.sock" }] }),
            ),
            (&["match"], json!([{ "forward": "unix//run/admin.sock" }])),
        ] {
            let error = set(path, value.clone());
            assert!(error.contains("unix socket"), "{value}: {error}");
        }
    }
}
