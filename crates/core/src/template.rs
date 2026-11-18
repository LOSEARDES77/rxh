//! Text with request placeholders, used by `rewrite` and `redirect`.

use std::str::FromStr;

use http::{HeaderMap, Uri};

use crate::matcher;

/// A string such as `https://{host}{uri}` whose placeholders are replaced
/// with parts of a request:
///
/// - `{host}`: host of the request, without the port.
/// - `{path}`: path of the URI.
/// - `{query}`: query string without `?`, empty when there is none.
/// - `{uri}`: path, plus `?query` when there is one.
///
/// Templates built with [`Template::for_response`] also take parts of an
/// upstream response:
///
/// - `{status}`: status code of the response.
/// - `{header.NAME}`: first value of the response header `NAME`, empty when
///   it is missing or not text.
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
    Status,
    Header(String),
}

impl FromStr for Template {
    type Err = String;

    /// Fails on unknown placeholders, on a `{` that is never closed and on
    /// control characters.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text, /* response= */ false)
    }
}

impl Template {
    /// Parses a template that may also use `{status}` and `{header.NAME}`.
    /// Fails like [`Template::from_str`], and on invalid header names.
    pub fn for_response(text: &str) -> Result<Self, String> {
        Self::parse(text, /* response= */ true)
    }

    fn parse(text: &str, response: bool) -> Result<Self, String> {
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
                "status" if response => Part::Status,
                other => match other.strip_prefix("header.") {
                    Some(name) if response => {
                        if http::HeaderName::from_bytes(name.as_bytes()).is_err() {
                            return Err(format!("invalid header name {name:?} in {text:?}"));
                        }
                        Part::Header(name.to_owned())
                    }
                    _ => {
                        return Err(format!(
                            "unknown placeholder {{{other}}} in {text:?}, expected {{host}}, \
                             {{path}}, {{query}} or {{uri}}"
                        ));
                    }
                },
            });
            rest = &rest[start + len + 1..];
        }
        if !rest.is_empty() {
            parts.push(Part::Text(rest.to_owned()));
        }
        Ok(Self { parts })
    }

    /// Fills the placeholders from a request to `host` (with or without
    /// port) for `uri`. Response placeholders are left empty.
    #[must_use]
    pub fn render(&self, host: &str, uri: &Uri) -> String {
        self.fill(host, uri, None)
    }

    /// Same as [`Template::render`], with `{status}` and `{header.NAME}`
    /// taken from the upstream response.
    #[must_use]
    pub fn render_response(
        &self,
        host: &str,
        uri: &Uri,
        status: u16,
        headers: &HeaderMap,
    ) -> String {
        self.fill(host, uri, Some((status, headers)))
    }

    fn fill(&self, host: &str, uri: &Uri, response: Option<(u16, &HeaderMap)>) -> String {
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
                Part::Status => {
                    if let Some((status, _)) = response {
                        text.push_str(&status.to_string());
                    }
                }
                Part::Header(name) => {
                    let value = response
                        .and_then(|(_, headers)| headers.get(name))
                        .and_then(|value| value.to_str().ok());
                    text.push_str(value.unwrap_or_default());
                }
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
    fn response_placeholders_are_filled_from_the_response() {
        let template = Template::for_response("/{status}.html|{header.X-File}|{path}").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-file", "/a.txt".parse().unwrap());
        let uri: Uri = "/x".parse().unwrap();
        assert_eq!(
            template.render_response("a.test", &uri, 503, &headers),
            "/503.html|/a.txt|/x"
        );
        assert_eq!(
            template.render_response("a.test", &uri, 404, &HeaderMap::new()),
            "/404.html||/x"
        );
    }

    #[test]
    fn response_placeholders_are_rejected_outside_response_templates() {
        for text in ["/{status}", "/{header.x-file}"] {
            assert!(text.parse::<Template>().is_err(), "{text:?}");
        }
        for text in ["/{header.}", "/{header.bad name}", "/{nope}"] {
            assert!(Template::for_response(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn invalid_templates_are_rejected() {
        for text in ["", "/a b", "/a\r\nx: y", "/{nope}", "/{path", "/{}"] {
            assert!(text.parse::<Template>().is_err(), "{text:?}");
        }
    }
}
