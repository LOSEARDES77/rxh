//! Text with request placeholders, used by `rewrite` and `redirect`.

use std::str::FromStr;

use http::Uri;

use crate::matcher;

/// A string such as `https://{host}{uri}` whose placeholders are replaced
/// with parts of a request:
///
/// - `{host}`: host of the request, without the port.
/// - `{path}`: path of the URI.
/// - `{query}`: query string without `?`, empty when there is none.
/// - `{uri}`: path, plus `?query` when there is one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Host,
    Path,
    Query,
    Uri,
}

impl FromStr for Template {
    type Err = String;

    /// Fails on unknown placeholders, on a `{` that is never closed and on
    /// control characters.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.is_empty() || text.contains(|c: char| c.is_control() || c == ' ') {
            return Err(format!(
                "{text:?} must not be empty nor contain spaces or control characters"
            ));
        }
        let mut parts = Vec::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let Some(len) = rest[start..].find('}') else {
                return Err(format!("unclosed placeholder in {text:?}"));
            };
            if start > 0 {
                parts.push(Part::Text(rest[..start].to_owned()));
            }
            parts.push(match &rest[start + 1..start + len] {
                "host" => Part::Host,
                "path" => Part::Path,
                "query" => Part::Query,
                "uri" => Part::Uri,
                other => {
                    return Err(format!(
                        "unknown placeholder {{{other}}} in {text:?}, expected {{host}}, \
                         {{path}}, {{query}} or {{uri}}"
                    ));
                }
            });
            rest = &rest[start + len + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Self { parts })
    }
}

impl Template {
    /// Fills the placeholders from a request to `host` (with or without
    /// port) for `uri`.
    #[must_use]
    pub fn render(&self, host: &str, uri: &Uri) -> String {
        let mut text = String::new();
        for part in &self.parts {
            match part {
                Part::Text(literal) => text.push_str(literal),
                Part::Host => text.push_str(matcher::strip_port(host)),
                Part::Path => text.push_str(uri.path()),
                Part::Query => text.push_str(uri.query().unwrap_or_default()),
                Part::Uri => text.push_str(
                    uri.path_and_query()
                        .map_or(uri.path(), |path_and_query| path_and_query.as_str()),
                ),
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(template: &str, host: &str, uri: &str) -> String {
        let template: Template = template.parse().unwrap();
        template.render(host, &uri.parse().unwrap())
    }

    #[test]
    fn placeholders_are_filled_from_the_request() {
        assert_eq!(
            render("https://{host}:8443{uri}", "a.test:80", "/x/y?q=1&r"),
            "https://a.test:8443/x/y?q=1&r"
        );
        assert_eq!(render("{path}|{query}|{uri}", "a.test", "/x"), "/x||/x");
        assert_eq!(
            render("/v2{path}?{query}&z=1", "", "/x?q=1"),
            "/v2/x?q=1&z=1"
        );
        assert_eq!(render("/fixed", "a.test", "/x?q=1"), "/fixed");
        assert_eq!(render("{host}", "[::1]:80", "/"), "[::1]");
    }

    #[test]
    fn invalid_templates_are_rejected() {
        for text in ["", "/a b", "/a\r\nx: y", "/{nope}", "/{path", "/{}"] {
            assert!(text.parse::<Template>().is_err(), "{text:?}");
        }
    }
}
