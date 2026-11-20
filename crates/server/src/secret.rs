//! Secret values of a configuration document: masked as [`MASK`] before the
//! document leaves the daemon, and put back from the stored document when a
//! [`MASK`] comes back.

use std::fmt;

use toml_edit::{Decor, DocumentMut, Item, Value};

/// What a secret reads as outside the daemon.
pub(crate) const MASK: &str = "***";

/// Paths of secret strings; `*` matches one key or every element of an array,
/// `**` matches zero or more of them (fallback chains, inline pools of
/// `forward` and `subroute` entries nest without limit).
pub(crate) const PATHS: &[&[&str]] = &[
    &["admin", "token"],
    &["consul", "token"],
    &["etcd", "token"],
    &["nomad", "token"],
    &["kubernetes", "token"],
    &["acme", "dns", "api_token"],
    &["acme", "dns", "secret_access_key"],
    &["acme", "dns", "tsig_secret"],
    &["upstream", "*", "secret"],
    &["upstream", "*", "**", "fallback", "secret"],
    &["server", "*", "**", "forward", "secret"],
    &["server", "*", "**", "forward", "**", "fallback", "secret"],
];

/// Replaces every secret of `document` written as a literal string with
/// [`MASK`]. A value with a placeholder (`{env.NAME}`, `{file.PATH}`) only
/// names where the secret is, and is left as written.
pub(crate) fn mask(document: &mut DocumentMut) {
    let secrets: Vec<Vec<Step>> = secrets(document.as_item(), |text| {
        !text.contains("{env.") && !text.contains("{file.")
    });
    for steps in secrets {
        replace(document.as_item_mut(), &steps, MASK);
    }
}

/// Puts the secret of `stored` wherever `edited` has [`MASK`].
///
/// Returns a message naming the path when `edited` has [`MASK`] where
/// `stored` has no secret.
pub(crate) fn restore(edited: &mut DocumentMut, stored: &DocumentMut) -> Result<(), String> {
    let masked: Vec<Vec<Step>> = secrets(edited.as_item(), |text| text == MASK);
    for steps in masked {
        let secret: Option<&str> = get(stored.as_item(), &steps).and_then(Item::as_str);
        match secret {
            Some(secret) => replace(edited.as_item_mut(), &steps, secret),
            None => {
                let names: Vec<String> = steps.iter().map(Step::to_string).collect();
                return Err(format!(
                    "{} is {MASK} but no value is stored",
                    names.join(".")
                ));
            }
        }
    }
    Ok(())
}

/// [`restore`] for a file the client sent back whole. The servers of a file
/// have no key but their position, so a [`MASK`] in a server is restored only
/// when that server is still the stored one at its position: same `name`, or
/// same `listen` without a name, and no other server of either document with
/// that name or address. The secret of a server that moved, or that cannot
/// be told from another, must be typed again.
pub(crate) fn restore_file(edited: &mut DocumentMut, stored: &DocumentMut) -> Result<(), String> {
    let edited_servers: Vec<Option<String>> = identities(edited);
    let stored_servers: Vec<Option<String>> = identities(stored);
    for steps in secrets(edited.as_item(), |text| text == MASK) {
        let [Step::Key(key), Step::Index(index), ..] = steps.as_slice() else {
            continue;
        };
        if key != "server" {
            continue;
        }
        let identity: Option<&Option<String>> = edited_servers.get(*index);
        if identity != stored_servers.get(*index) {
            return Err(format!(
                "server {index} is not the server stored at that position, type its secret again \
                 instead of {MASK}"
            ));
        }
        let is_unique = |servers: &[Option<String>]| {
            servers
                .iter()
                .filter(|other| Some(*other) == identity)
                .count()
                == 1
        };
        if !is_unique(&edited_servers) || !is_unique(&stored_servers) {
            return Err(format!(
                "server {index} has the name or the address of another server, type its secret \
                 again instead of {MASK}"
            ));
        }
    }
    restore(edited, stored)
}

/// What tells each server of `document` from the others: its `name`, else
/// its `listen` as written.
fn identities(document: &DocumentMut) -> Vec<Option<String>> {
    let Some(servers) = document.get("server") else {
        return Vec::new();
    };
    children(servers)
        .into_iter()
        .map(|(_, server)| {
            let value: &Value = server
                .get("name")
                .or_else(|| server.get("listen"))?
                .as_value()?;
            Some(value.to_string().trim().to_owned())
        })
        .collect()
}

/// One key of a table or one index of an array.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    Key(String),
    Index(usize),
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Key(key) => f.write_str(key),
            Step::Index(index) => write!(f, "{index}"),
        }
    }
}

/// Where the strings of [`PATHS`] that `wanted` accepts are, each once.
fn secrets(root: &Item, wanted: impl Fn(&str) -> bool) -> Vec<Vec<Step>> {
    let mut found: Vec<Vec<Step>> = Vec::new();
    for path in PATHS {
        walk(root, path, &mut Vec::new(), &mut |item, steps| {
            if item.as_str().is_some_and(&wanted) && !found.iter().any(|seen| seen == steps) {
                found.push(steps.to_vec());
            }
        });
    }
    found
}

/// Calls `visit` with every item `path` reaches from `item`, and its steps.
fn walk(item: &Item, path: &[&str], steps: &mut Vec<Step>, visit: &mut dyn FnMut(&Item, &[Step])) {
    let Some((&segment, rest)) = path.split_first() else {
        visit(item, steps);
        return;
    };
    if segment == "**" {
        walk(item, rest, steps, visit);
    }
    for (step, child) in children(item) {
        let next: &[&str] = match segment {
            "**" => path,
            "*" => rest,
            _ if step == Step::Key(segment.to_owned()) => rest,
            _ => continue,
        };
        steps.push(step);
        walk(child, next, steps, visit);
        steps.pop();
    }
}

/// The entries of a table of any layout, or the elements of an array.
fn children(item: &Item) -> Vec<(Step, &Item)> {
    if let Some(table) = item.as_table_like() {
        return table
            .iter()
            .map(|(key, child)| (Step::Key(key.to_owned()), child))
            .collect();
    }
    let len: usize = match item {
        Item::ArrayOfTables(tables) => tables.len(),
        Item::Value(Value::Array(array)) => array.len(),
        _ => 0,
    };
    (0..len)
        .filter_map(|index| Some((Step::Index(index), item.get(index)?)))
        .collect()
}

fn get<'d>(mut item: &'d Item, steps: &[Step]) -> Option<&'d Item> {
    for step in steps {
        item = match step {
            Step::Key(key) => item.get(key)?,
            Step::Index(index) => item.get(index)?,
        };
    }
    Some(item)
}

/// Sets the string at `steps`, which must exist, keeping its formatting.
fn replace(mut item: &mut Item, steps: &[Step], text: &str) {
    for step in steps {
        item = match step {
            Step::Key(key) => item.get_mut(key),
            Step::Index(index) => item.get_mut(index),
        }
        .expect("the steps come from a walk of this document");
    }
    let value: &mut Value = item.as_value_mut().expect("the walk only keeps strings");
    let decor: Decor = value.decor().clone();
    *value = Value::from(text);
    *value.decor_mut() = decor;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use rxh_core::config::Config;
    use serde_json::Value as Json;

    use super::*;

    fn parse(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    fn masked(text: &str) -> DocumentMut {
        let mut document: DocumentMut = parse(text);
        mask(&mut document);
        document
    }

    #[test]
    fn masks_literals_not_placeholders() {
        let document: DocumentMut = masked(
            r#"
[admin]
token = "s3cret-token-of-the-admin-api"
listen = "127.0.0.1:9000"

[acme.dns]
provider = "cloudflare"
api_token = "{env.CF_TOKEN}"
tsig_key = "rxh-key"
tsig_secret = "{file./run/secrets/tsig}"
secret_access_key = "aws-secret"
"#,
        );
        assert_eq!(document["admin"]["token"].as_str(), Some(MASK));
        assert_eq!(document["admin"]["listen"].as_str(), Some("127.0.0.1:9000"));
        assert_eq!(
            document["acme"]["dns"]["api_token"].as_str(),
            Some("{env.CF_TOKEN}")
        );
        assert_eq!(
            document["acme"]["dns"]["tsig_secret"].as_str(),
            Some("{file./run/secrets/tsig}")
        );
        assert_eq!(
            document["acme"]["dns"]["tsig_key"].as_str(),
            Some("rxh-key")
        );
        assert_eq!(
            document["acme"]["dns"]["secret_access_key"].as_str(),
            Some(MASK)
        );
    }

    #[test]
    fn masks_inline_tables_and_dotted_keys() {
        let document: DocumentMut = masked(
            r#"
admin = { token = "inline-secret" }
consul.token = "dotted-secret"
upstream.api = { backends = ["127.0.0.1:8080"], policy = "cookie", secret = "pool" }
"#,
        );
        let text: String = document.to_string();
        assert!(!text.contains("inline-secret"), "{text}");
        assert!(!text.contains("dotted-secret"), "{text}");
        assert!(!text.contains("\"pool\""), "{text}");
        assert!(
            text.contains(r#"admin = { token = "***" }"#),
            "formatting must be kept: {text}"
        );
    }

    #[test]
    fn restore_puts_back_the_stored_secret() {
        let text: &str = r#"
# The token of the admin API.
[admin]
token = "s3cret-token-of-the-admin-api" # rotated monthly

[upstream.api]
backends = ["127.0.0.1:8080"]
policy = "cookie"
secret = "cookie-hmac"
"#;
        let stored: DocumentMut = parse(text);
        let mut edited: DocumentMut = masked(text);
        restore(&mut edited, &stored).unwrap();
        assert_eq!(edited.to_string(), text);
    }

    #[test]
    fn restore_keeps_a_newly_typed_secret() {
        let stored: DocumentMut = parse("[admin]\ntoken = \"old-token-of-the-admin\"\n");
        let mut edited: DocumentMut = parse("[admin]\ntoken = \"new-token-of-the-admin\"\n");
        restore(&mut edited, &stored).unwrap();
        assert_eq!(
            edited["admin"]["token"].as_str(),
            Some("new-token-of-the-admin")
        );
    }

    #[test]
    fn mask_without_stored_value_is_an_error() {
        let stored: DocumentMut = parse("[admin]\nlisten = \"127.0.0.1:9000\"\n");
        let mut edited: DocumentMut = parse("[admin]\ntoken = \"***\"\n");
        assert_eq!(
            restore(&mut edited, &stored),
            Err("admin.token is *** but no value is stored".to_owned())
        );

        let stored: DocumentMut = parse("[[server]]\nlisten = \"127.0.0.1:80\"\n");
        let mut edited: DocumentMut = parse(
            "[[server]]\nlisten = \"127.0.0.1:80\"\nforward = { backends = [\"127.0.0.1:8080\"], \
             secret = \"***\" }\n",
        );
        assert_eq!(
            restore(&mut edited, &stored),
            Err("server.0.forward.secret is *** but no value is stored".to_owned())
        );
    }

    #[test]
    fn a_file_restores_a_server_only_at_its_own_position() {
        let server = |name: &str, secret: &str| {
            format!(
                "[[server]]\nname = \"{name}\"\nlisten = \"127.0.0.1:80\"\nforward = {{ backends = \
                 [\"127.0.0.1:1\"], policy = \"cookie\", secret = \"{secret}\" }}\n"
            )
        };
        let text: String = server("a", "secret-of-a") + &server("b", "secret-of-b");
        let stored: DocumentMut = parse(&text);

        let mut same_order: DocumentMut = masked(&text);
        restore_file(&mut same_order, &stored).unwrap();
        assert_eq!(same_order.to_string(), text);

        let mut swapped: DocumentMut = parse(&(server("b", MASK) + &server("a", MASK)));
        let err: String = restore_file(&mut swapped, &stored).unwrap_err();
        assert!(err.contains("server 0"), "{err}");
        assert!(
            !swapped.to_string().contains("secret-of"),
            "nothing restored"
        );

        // Without names the listen address tells them apart.
        let unnamed = |listen: &str, secret: &str| {
            format!(
                "[[server]]\nlisten = \"{listen}\"\nforward = {{ backends = [\"127.0.0.1:1\"], \
                 secret = \"{secret}\" }}\n"
            )
        };
        let stored: DocumentMut = parse(&unnamed("127.0.0.1:80", "secret-of-80"));
        let mut moved: DocumentMut = parse(&unnamed("127.0.0.1:81", MASK));
        assert!(restore_file(&mut moved, &stored).is_err());
        let mut kept: DocumentMut = parse(&unnamed("127.0.0.1:80", MASK));
        restore_file(&mut kept, &stored).unwrap();
        assert!(kept.to_string().contains("secret-of-80"));

        // Two servers on one address cannot be told apart, in place or swapped.
        let twins: String = unnamed("127.0.0.1:80", "secret-of-first")
            + &unnamed("127.0.0.1:80", "secret-of-second");
        let stored_twins: DocumentMut = parse(&twins);
        let mut masked_twins: DocumentMut = masked(&twins);
        let err: String = restore_file(&mut masked_twins, &stored_twins).unwrap_err();
        assert!(err.contains("of another server"), "{err}");
        assert!(
            !masked_twins.to_string().contains("secret-of"),
            "nothing restored"
        );
        // One in the edited file only is enough to refuse.
        let mut doubled: DocumentMut =
            parse(&(unnamed("127.0.0.1:80", MASK) + &unnamed("127.0.0.1:80", "typed")));
        assert!(restore_file(&mut doubled, &stored).is_err());
        // Typed secrets need no stored server.
        let mut typed_twins: DocumentMut =
            parse(&(unnamed("127.0.0.1:80", "x") + &unnamed("127.0.0.1:80", "y")));
        restore_file(&mut typed_twins, &stored_twins).unwrap();

        // A new secret typed in a server that moved is fine.
        let mut typed: DocumentMut = parse(&unnamed("127.0.0.1:81", "typed-again"));
        restore_file(&mut typed, &stored).unwrap();
    }

    #[test]
    fn wildcards_reach_every_pool_and_server() {
        let text: &str = r#"
kubernetes = { token = "k8s" }

[upstream.a]
backends = ["127.0.0.1:8080"]
secret = "pool-a"
fallback = { policy = "cookie", secret = "fallback-1", fallback = { policy = "cookie", secret = "fallback-2" } }

[upstream.b]
backends = ["127.0.0.1:8081"]
secret = "pool-b"

[[server]]
listen = "127.0.0.1:80"
forward = { backends = ["127.0.0.1:8080"], policy = "cookie", secret = "server-0" }

[[server]]
listen = "127.0.0.1:81"
protocol = "tcp"
match = [
    { subroute = [{ forward = { backends = ["127.0.0.1:8080"], secret = "nested" } }] },
]
"#;
        let document: DocumentMut = masked(text);
        let masked_text: String = document.to_string();
        for secret in [
            "k8s",
            "pool-a",
            "pool-b",
            "fallback-1",
            "fallback-2",
            "server-0",
            "nested",
        ] {
            assert!(
                !masked_text.contains(&format!("\"{secret}\"")),
                "{secret}: {masked_text}"
            );
        }
        let mut edited: DocumentMut = document;
        restore(&mut edited, &parse(text)).unwrap();
        assert_eq!(edited.to_string(), text);

        let array: DocumentMut = masked(
            r#"server = [{ listen = "127.0.0.1:80", forward = { backends = ["127.0.0.1:1"], secret = "arr" } }]"#,
        );
        assert_eq!(array["server"][0]["forward"]["secret"].as_str(), Some(MASK));
    }

    #[test]
    fn every_secret_field_of_the_schema_is_listed() {
        const SECRET_NAMES: [&str; 6] = [
            "token",
            "secret",
            "api_token",
            "secret_access_key",
            "tsig_secret",
            "password",
        ];
        fn collect(json: &Json, names: &mut BTreeSet<String>) {
            match json {
                Json::Object(object) => {
                    if let Some(Json::Object(properties)) = object.get("properties") {
                        names.extend(properties.keys().cloned());
                    }
                    for value in object.values() {
                        collect(value, names);
                    }
                }
                Json::Array(values) => {
                    for value in values {
                        collect(value, names);
                    }
                }
                _ => {}
            }
        }
        let mut names: BTreeSet<String> = BTreeSet::new();
        collect(Config::json_schema().as_value(), &mut names);
        let secrets: Vec<&String> = names
            .iter()
            .filter(|name| SECRET_NAMES.contains(&name.as_str()))
            .collect();
        assert!(
            !secrets.is_empty(),
            "the schema walk must find the known secrets"
        );
        for name in secrets {
            assert!(
                PATHS.iter().any(|path| path.last() == Some(&name.as_str())),
                "{name} is a secret of the schema, it must be masked"
            );
        }
    }
}
