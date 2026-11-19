//! HTTP status matchers such as `503` or `5xx`.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Matches one status code or a whole class (`2xx`, `5xx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "StatusOption", into = "String")]
pub enum StatusMatcher {
    /// A single code, like `404`.
    Code(u16),

    /// A class of codes, `5` for `5xx`.
    Class(u16),
}

impl StatusMatcher {
    /// Returns true if `status` matches.
    #[must_use]
    pub fn matches(self, status: u16) -> bool {
        match self {
            StatusMatcher::Code(code) => code == status,
            StatusMatcher::Class(class) => status / 100 == class,
        }
    }
}

impl Default for StatusMatcher {
    fn default() -> Self {
        StatusMatcher::Class(2)
    }
}

impl FromStr for StatusMatcher {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid =
            || format!("invalid status {text:?}, expected a code like 503 or a class like 5xx");
        let lower = text.to_ascii_lowercase();

        if let Some(class) = lower.strip_suffix("xx") {
            let class: u16 = class.parse().map_err(|_| invalid())?;
            return if (1..=5).contains(&class) {
                Ok(StatusMatcher::Class(class))
            } else {
                Err(invalid())
            };
        }

        let code: u16 = lower.parse().map_err(|_| invalid())?;
        if (100..=599).contains(&code) {
            Ok(StatusMatcher::Code(code))
        } else {
            Err(invalid())
        }
    }
}

impl fmt::Display for StatusMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StatusMatcher::Code(code) => write!(f, "{code}"),
            StatusMatcher::Class(class) => write!(f, "{class}xx"),
        }
    }
}

impl From<StatusMatcher> for String {
    fn from(value: StatusMatcher) -> Self {
        value.to_string()
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum StatusOption {
    Number(u16),
    Text(String),
}

impl TryFrom<StatusOption> for StatusMatcher {
    type Error = String;

    fn try_from(value: StatusOption) -> Result<Self, Self::Error> {
        match value {
            StatusOption::Number(code) => code.to_string().parse(),
            StatusOption::Text(text) => text.parse(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let class: StatusMatcher = "5xx".parse().unwrap();
        assert!(class.matches(503));
        assert!(!class.matches(404));

        let code: StatusMatcher = "404".parse().unwrap();
        assert!(code.matches(404));
        assert!(!code.matches(400));

        assert!("6xx".parse::<StatusMatcher>().is_err());
        assert!("99".parse::<StatusMatcher>().is_err());
    }
}
