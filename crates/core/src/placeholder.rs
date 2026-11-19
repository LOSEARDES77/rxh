//! `{env.NAME}` and `{file.PATH}` placeholders in configuration files.
//!
//! Placeholders are replaced in TOML string values before the file is
//! deserialized, so a secret can live in an environment variable or a file
//! instead of the configuration:
//!
//! ```toml
//! [admin]
//! token = "{file./run/secrets/rxh-admin-token}"
//!
//! [upstream.api]
//! backends = ["{env.API_HOST}:8080"]
//! ```
//!
//! Keys, numbers and booleans are left alone, and the value is inserted as
//! string content, so quotes or newlines in it cannot change the structure
//! of the document. Other `{...}` sequences, such as the request placeholders
//! of `rewrite`, are kept as written.

use std::borrow::Cow;

use toml_edit::visit_mut::VisitMut;
use toml_edit::{DocumentMut, Formatted};

use crate::config::ConfigError;

/// Source of the value of one placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
    /// `{env.NAME}`: the environment variable `NAME`.
    Env(&'a str),

    /// `{file.PATH}`: the contents of the file at `PATH`.
    File(&'a str),
}

/// Replaces the placeholders in the string values of the TOML document
/// `text`, asking `resolve` for each value. `resolve` returns an error
/// message when the value is not available.
///
/// A document without placeholders is returned as is. One that is not valid
/// TOML is returned as is too, so the parser that follows reports the error
/// with its usual message.
pub fn expand<F>(text: &str, resolve: F) -> Result<Cow<'_, str>, ConfigError>
where
    F: FnMut(Lookup<'_>) -> Result<String, String>,
{
    if !text.contains("{env.") && !text.contains("{file.") {
        return Ok(Cow::Borrowed(text));
    }
    let Ok(mut document) = text.parse::<DocumentMut>() else {
        return Ok(Cow::Borrowed(text));
    };
    let mut expander = Expander {
        resolve,
        error: None,
    };
    expander.visit_document_mut(&mut document);
    match expander.error {
        Some(message) => Err(ConfigError::Invalid(message)),
        None => Ok(Cow::Owned(document.to_string())),
    }
}

struct Expander<F> {
    resolve: F,
    /// First failure; the remaining strings are skipped.
    error: Option<String>,
}

impl<F> VisitMut for Expander<F>
where
    F: FnMut(Lookup<'_>) -> Result<String, String>,
{
    fn visit_string_mut(&mut self, node: &mut Formatted<String>) {
        if self.error.is_some() {
            return;
        }
        match expand_string(node.value(), &mut self.resolve) {
            Ok(None) => {}
            Ok(Some(expanded)) => {
                let decor = node.decor().clone();
                *node = Formatted::new(expanded);
                *node.decor_mut() = decor;
            }
            Err(message) => self.error = Some(message),
        }
    }
}

/// The expansion of one string value, `None` when it has no placeholder.
fn expand_string<F>(value: &str, resolve: &mut F) -> Result<Option<String>, String>
where
    F: FnMut(Lookup<'_>) -> Result<String, String>,
{
    let mut output = String::new();
    let mut rest = value;
    let mut found = false;
    while let Some(start) = next_placeholder(rest) {
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find('}')
            .ok_or_else(|| format!("placeholder in {value:?} has no closing }}"))?;
        let placeholder = &tail[..=end];
        let lookup = if let Some(name) = tail[..end].strip_prefix("{env.") {
            let valid = !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            if !valid {
                return Err(format!(
                    "{placeholder}: an environment variable name has only letters, digits and _"
                ));
            }
            Lookup::Env(name)
        } else {
            let path = &tail["{file.".len()..end];
            if path.is_empty() {
                return Err(format!("{placeholder}: the file path is empty"));
            }
            Lookup::File(path)
        };
        let resolved = resolve(lookup).map_err(|problem| format!("{placeholder}: {problem}"))?;
        output.push_str(&resolved);
        rest = &tail[end + 1..];
        found = true;
    }
    if !found {
        return Ok(None);
    }
    output.push_str(rest);
    Ok(Some(output))
}

/// Byte offset of the next `{env.` or `{file.` in `text`.
fn next_placeholder(text: &str) -> Option<usize> {
    let env = text.find("{env.");
    let file = text.find("{file.");
    match (env, file) {
        (Some(env), Some(file)) => Some(env.min(file)),
        (one, other) => one.or(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn resolve(lookup: Lookup<'_>) -> Result<String, String> {
        match lookup {
            Lookup::Env("HOST") => Ok(String::from("10.0.0.1")),
            Lookup::Env("EMPTY") => Ok(String::new()),
            Lookup::Env("TRICKY") => Ok(String::from("a\"b\\c\nd'e")),
            Lookup::File("/run/secrets/token") => Ok(String::from("0123456789abcdef")),
            Lookup::Env(name) => Err(format!("environment variable {name} is not set")),
            Lookup::File(path) => Err(format!("cannot read {path}")),
        }
    }

    fn expanded(text: &str) -> Result<String, String> {
        expand(text, resolve)
            .map(Cow::into_owned)
            .map_err(|err| err.to_string())
    }

    #[test]
    fn replaces_env_and_file_placeholders_in_string_values() {
        let text = "# kept\n[admin]\ntoken = \"{file./run/secrets/token}\" # kept too\n\n\
                    [upstream.api]\nbackends = [\"{env.HOST}:80\", 'x{env.EMPTY}y']\n\
                    hosts = { \"{env.HOST}\" = \"{env.HOST}\" }\n";
        assert_eq!(
            expanded(text).unwrap(),
            "# kept\n[admin]\ntoken = \"0123456789abcdef\" # kept too\n\n\
             [upstream.api]\nbackends = [\"10.0.0.1:80\", \"xy\"]\n\
             hosts = { \"{env.HOST}\" = \"10.0.0.1\" }\n"
        );
    }

    #[test]
    fn values_cannot_change_the_document_structure() {
        let text = expanded("a = \"{env.TRICKY}\"\nb = 1\n").unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(table["a"].as_str(), Some("a\"b\\c\nd'e"));
        assert_eq!(table["b"].as_integer(), Some(1));
        assert_eq!(table.len(), 2);
    }

    #[test]
    fn other_braces_and_documents_without_placeholders_are_untouched() {
        let text = "rewrite = \"/v2{uri}\"\nx = \"{envy}\"\n";
        assert!(matches!(expand(text, resolve).unwrap(), Cow::Borrowed(_)));
        let mixed = "a = \"{host}{env.HOST}{path}\"\n";
        assert_eq!(expanded(mixed).unwrap(), "a = \"{host}10.0.0.1{path}\"\n");
        // Invalid TOML is left for the real parser to report.
        assert_eq!(expanded("a = \"{env.X}").unwrap(), "a = \"{env.X}");
    }

    #[test]
    fn rejects_unknown_and_malformed_placeholders() {
        for (text, message) in [
            (
                "a = \"{env.MISSING}\"",
                "{env.MISSING}: environment variable MISSING is not set",
            ),
            ("a = \"{file./nope}\"", "{file./nope}: cannot read /nope"),
            ("a = \"{env.HOST\"", "has no closing }"),
            ("a = \"{env.}\"", "{env.}: an environment variable name"),
            (
                "a = \"{env.A-B}\"",
                "{env.A-B}: an environment variable name",
            ),
            ("a = \"{file.}\"", "{file.}: the file path is empty"),
        ] {
            let err = expanded(text).unwrap_err();
            assert!(err.contains(message), "{text}: {err}");
        }
    }

    #[test]
    fn expanded_configuration_is_validated() {
        let config = "[admin]\ntoken = \"{env.HOST}\"\n\
                      [[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{env.HOST}:80\"\n";
        let err = Config::from_toml(&expanded(config).unwrap()).unwrap_err();
        assert!(
            err.to_string().contains("token must have at least"),
            "{err}"
        );

        let config = config.replace("{env.HOST}\"\n[[", "{file./run/secrets/token}\"\n[[");
        let parsed = Config::from_toml(&expanded(&config).unwrap()).unwrap();
        assert_eq!(parsed.admin.token.as_deref(), Some("0123456789abcdef"));
    }
}
