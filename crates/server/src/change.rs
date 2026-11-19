//! Configuration changes sent to the admin API: a whole configuration
//! (`POST /load`) or a JSON merge patch (RFC 7396) of one pool or server
//! (`PATCH /config/upstream/{name}`, `PATCH /config/server/{name or index}`).
//!
//! A patch is merged into the pool or server as `GET /config` prints it, so
//! it uses those keys: `policy` rather than the `lb_policy` alias, and the
//! routes of a server under `match` even when the file has a bare `forward`.
//!
//! Changes run in memory only: the configuration file is not written, and
//! the next reload or restart goes back to it.

use rxh_core::config::Config;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Part of the configuration a patch addresses, named like its key in
/// `GET /config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Upstream,
    Server,
}

impl Section {
    pub(crate) fn from_segment(segment: &str) -> Option<Self> {
        match segment {
            "upstream" => Some(Self::Upstream),
            "server" => Some(Self::Server),
            _ => None,
        }
    }
}

/// A change to the running configuration.
#[derive(Debug)]
pub(crate) enum Change {
    /// Replace everything but `[admin]`.
    Load(Box<Config>),

    /// Merge `patch` into the pool or server called `key`.
    Patch {
        section: Section,
        key: String,
        patch: Value,
    },
}

impl Change {
    /// The validated configuration to run after this change to `running`.
    pub(crate) fn apply(self, running: &Config) -> Result<Config, String> {
        let config = match self {
            Self::Load(config) => {
                let mut config = *config;
                if !config.include.is_empty() {
                    return Err(String::from(
                        "include is not followed in an uploaded configuration, send the merged \
                         one (GET /config)",
                    ));
                }
                // The listener and its token are fixed at startup, and
                // `GET /config` hands out the token redacted.
                config.admin = running.admin.clone();
                if let (Some(dns), Some(running)) = (&mut config.acme.dns, &running.acme.dns) {
                    dns.restore_redacted(running);
                }
                config
            }
            Self::Patch {
                section,
                key,
                patch,
            } => {
                let mut config = running.clone();
                match section {
                    Section::Upstream => {
                        let group = config
                            .upstreams
                            .get_mut(&key)
                            .ok_or_else(|| missing(section, &key))?;
                        *group = patched(group, patch)?;
                    }
                    Section::Server => {
                        let index =
                            server_index(&config, &key).ok_or_else(|| missing(section, &key))?;
                        config.servers[index] = patched(&config.servers[index], patch)?;
                    }
                }
                config
            }
        };
        config.validate().map_err(|err| err.to_string())?;
        Ok(config)
    }
}

/// Whether `config` has the pool or server a patch addresses.
pub(crate) fn exists(config: &Config, section: Section, key: &str) -> bool {
    match section {
        Section::Upstream => config.upstreams.contains_key(key),
        Section::Server => server_index(config, key).is_some(),
    }
}

/// Error message for a patch of something that is not configured.
pub(crate) fn missing(section: Section, key: &str) -> String {
    match section {
        Section::Upstream => format!("no pool named {key}"),
        Section::Server => format!("no server named {key}"),
    }
}

/// Position of the server with that `name`, else of the server at that
/// index.
fn server_index(config: &Config, key: &str) -> Option<usize> {
    let named = config
        .servers
        .iter()
        .position(|server| server.name.as_deref() == Some(key));
    named.or_else(|| {
        let index: usize = key.parse().ok()?;
        (index < config.servers.len()).then_some(index)
    })
}

/// Parses a request body: JSON when it starts with `{`, which a TOML
/// document cannot, else TOML.
pub(crate) fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, String> {
    let text = str::from_utf8(body).map_err(|err| format!("body is not UTF-8: {err}"))?;
    if text.trim_start().starts_with('{') {
        serde_json::from_str(text).map_err(|err| format!("invalid JSON body: {err}"))
    } else {
        toml::from_str(text).map_err(|err| format!("invalid TOML body: {err}"))
    }
}

fn patched<T: Serialize + DeserializeOwned>(current: &T, patch: Value) -> Result<T, String> {
    let mut value = serde_json::to_value(current).map_err(|err| err.to_string())?;
    merge(&mut value, patch);
    serde_json::from_value(value)
        .map_err(|err| format!("invalid patch: {err} (use the keys GET /config shows)"))
}

/// JSON merge patch (RFC 7396): objects merge key by key, `null` removes a
/// key, anything else replaces the target.
fn merge(target: &mut Value, patch: Value) {
    let Value::Object(patch) = patch else {
        *target = patch;
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    let Value::Object(target) = target else {
        unreachable!("the target was made an object just above");
    };
    for (key, value) in patch {
        if value.is_null() {
            target.remove(&key);
        } else {
            merge(target.entry(key).or_insert(Value::Null), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const RUNNING: &str = r#"
[admin]
listen = "127.0.0.1:2019"
token = "0123456789abcdef"

[acme.dns]
provider = "cloudflare"
api_token = "real-secret"

[upstream.api]
backends = ["127.0.0.1:1", "127.0.0.1:2"]
lb_policy = "least_conn"
health = { uri = "/up", interval = "5s" }

[[server]]
name = "web"
listen = "127.0.0.1:8080"
forward = "@api"

[[server]]
listen = "127.0.0.1:8081"
forward = "127.0.0.1:3"
"#;

    fn running() -> Config {
        Config::from_toml(RUNNING).unwrap()
    }

    fn patch(section: Section, key: &str, patch: Value) -> Result<Config, String> {
        let change = Change::Patch {
            section,
            key: key.to_owned(),
            patch,
        };
        change.apply(&running())
    }

    fn json_of(config: &Config) -> Value {
        serde_json::to_value(config).unwrap()
    }

    #[test]
    fn merge_patch_follows_rfc_7396() {
        let mut target = json!({ "a": "b", "c": { "d": "e", "f": "g" }, "list": [1, 2] });
        merge(
            &mut target,
            json!({ "a": "z", "c": { "f": null, "h": 1 }, "list": [3], "new": { "x": null } }),
        );
        assert_eq!(
            target,
            json!({ "a": "z", "c": { "d": "e", "h": 1 }, "list": [3], "new": {} })
        );
    }

    #[test]
    fn shipped_examples_survive_json() {
        // `GET /config` must be accepted back by `POST /load`, and patches
        // go through JSON.
        let examples = [
            include_str!("../../../examples/full.toml"),
            include_str!("../../../examples/http-load-balancer.toml"),
            include_str!("../../../examples/tcp-udp.toml"),
        ];
        for example in examples {
            let config = Config::from_toml(example).unwrap();
            let json = serde_json::to_string(&config).unwrap();
            let back: Config = parse(json.as_bytes()).unwrap();
            back.validate().unwrap();
            assert_eq!(json_of(&back), json_of(&config));
        }
    }

    #[test]
    fn bodies_are_json_or_toml() {
        let from_toml: Config = parse(b"watch = true\n").unwrap();
        assert!(from_toml.watch);
        let from_json: Config = parse(b"  {\"watch\": true}").unwrap();
        assert!(from_json.watch);

        let err = parse::<Config>(b"{\"watch\": 1}").unwrap_err();
        assert!(err.starts_with("invalid JSON body"), "{err}");
        let err = parse::<Config>(b"watch = 1").unwrap_err();
        assert!(err.starts_with("invalid TOML body"), "{err}");
        let err = parse::<Config>(b"nope = 1").unwrap_err();
        assert!(err.contains("nope"), "unknown keys are rejected: {err}");
        let err = parse::<Config>(&[0xff]).unwrap_err();
        assert!(err.starts_with("body is not UTF-8"), "{err}");
    }

    #[test]
    fn load_keeps_admin_and_redacted_secrets() {
        let uploaded = "[admin]\nenabled = false\n\
            [acme.dns]\nprovider = \"cloudflare\"\napi_token = \"***\"\n\
            [[server]]\nlisten = \"127.0.0.1:9000\"\nforward = \"127.0.0.1:4\"\n";
        let change = Change::Load(Box::new(parse(uploaded.as_bytes()).unwrap()));
        let config = change.apply(&running()).unwrap();
        assert!(config.admin.enabled);
        assert_eq!(config.admin.token.as_deref(), Some("0123456789abcdef"));
        let dns = config.acme.dns.expect("dns");
        assert_eq!(dns.api_token.as_deref(), Some("real-secret"));
        assert_eq!(config.servers.len(), 1);
        assert!(config.upstreams.is_empty());
    }

    #[test]
    fn load_rejects_includes_and_invalid_configurations() {
        let load = |text: &str| Change::Load(Box::new(parse(text.as_bytes()).unwrap()));
        let err = load("include = [\"conf.d/*.toml\"]\n")
            .apply(&running())
            .unwrap_err();
        assert!(err.contains("include"), "{err}");
        let err = load("[[server]]\nlisten = \"127.0.0.1:9000\"\nforward = \"@missing\"\n")
            .apply(&running())
            .unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }

    #[test]
    fn patch_changes_only_the_named_keys_of_a_pool() {
        let config = patch(
            Section::Upstream,
            "api",
            json!({ "policy": "random", "health": { "interval": "9s" } }),
        )
        .unwrap();
        let api = &json_of(&config)["upstream"]["api"];
        let before = &json_of(&running())["upstream"]["api"];
        assert_eq!(api["policy"], "random");
        assert_eq!(api["health"]["interval"], "9s");
        assert_eq!(api["health"]["uri"], "/up", "untouched keys are kept");
        assert_eq!(api["backends"], before["backends"]);

        // `null` goes back to the default, arrays are replaced whole.
        let config = patch(
            Section::Upstream,
            "api",
            json!({ "health": null, "backends": ["127.0.0.1:9"] }),
        )
        .unwrap();
        let api = &config.upstreams["api"];
        assert!(api.health.is_none());
        assert_eq!(api.backends.len(), 1);
        assert_eq!(api.backends[0].address, "127.0.0.1:9");
    }

    #[test]
    fn patch_finds_servers_by_name_then_index() {
        let config = patch(
            Section::Server,
            "web",
            json!({ "listen": "127.0.0.1:9090" }),
        )
        .unwrap();
        let web = &json_of(&config)["server"][0];
        assert_eq!(web["listen"], json!(["127.0.0.1:9090"]));
        assert_eq!(
            web["match"][0]["forward"], "@api",
            "untouched keys are kept"
        );

        let config = patch(Section::Server, "1", json!({ "name": "second" })).unwrap();
        assert_eq!(config.servers[1].name.as_deref(), Some("second"));
        assert_eq!(config.servers[0].name.as_deref(), Some("web"));

        assert!(exists(&running(), Section::Server, "web"));
        assert!(exists(&running(), Section::Server, "0"));
        assert!(!exists(&running(), Section::Server, "2"));
        assert!(exists(&running(), Section::Upstream, "api"));
        assert!(!exists(&running(), Section::Upstream, "web#0"));
    }

    #[test]
    fn patch_rejects_what_the_file_would_reject() {
        let err = patch(Section::Upstream, "nope", json!({})).unwrap_err();
        assert_eq!(err, "no pool named nope");
        let err = patch(Section::Server, "7", json!({})).unwrap_err();
        assert_eq!(err, "no server named 7");
        let err = patch(Section::Upstream, "api", json!({ "polcy": "random" })).unwrap_err();
        assert!(
            err.starts_with("invalid patch") && err.contains("polcy"),
            "{err}"
        );
        // An alias next to the key it stands for.
        let err = patch(Section::Upstream, "api", json!({ "lb_policy": "random" })).unwrap_err();
        assert!(err.contains("use the keys GET /config shows"), "{err}");
        let err = patch(Section::Upstream, "api", json!({ "policy": 3 })).unwrap_err();
        assert!(err.starts_with("invalid patch"), "{err}");
        // Parses, but does not validate: the pool is gone for the server.
        let err = patch(
            Section::Server,
            "web",
            json!({ "match": [{ "forward": "@gone" }] }),
        )
        .unwrap_err();
        assert!(err.contains("gone"), "{err}");
    }
}
