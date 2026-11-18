//! Human readable durations such as `250ms`, `5s`, `1m30s` or `2h`.
//!
//! Durations in the configuration file use the same syntax as Caddy and Go:
//! a sequence of decimal numbers, each one followed by a unit (`ns`, `us`,
//! `ms`, `s`, `m`, `h`, `d`). A bare integer is read as seconds.

use std::fmt;
use std::time::Duration;

use serde::de;
use serde::{Deserialize, Deserializer, Serializer};

/// Error returned when a duration string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseDurationError(String);

impl fmt::Display for ParseDurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid duration {:?}, expected something like 250ms, 5s or 1m",
            self.0
        )
    }
}

impl std::error::Error for ParseDurationError {}

/// Parses a duration like `1m30s`. A plain number means seconds.
pub fn parse(input: &str) -> Result<Duration, ParseDurationError> {
    let error = || ParseDurationError(input.to_owned());
    let text = input.trim();

    if text.is_empty() {
        return Err(error());
    }

    if let Ok(seconds) = text.parse::<u64>() {
        return Ok(Duration::from_secs(seconds));
    }

    let mut total = Duration::ZERO;
    let mut rest = text;

    while !rest.is_empty() {
        let number_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(error)?;
        if number_len == 0 {
            return Err(error());
        }
        let value: f64 = rest[..number_len].parse().map_err(|_| error())?;
        rest = &rest[number_len..];

        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let nanos_per_unit = match &rest[..unit_len] {
            "ns" => 1.0,
            "us" | "µs" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3600e9,
            "d" => 86400e9,
            _ => return Err(error()),
        };
        rest = &rest[unit_len..];

        total += Duration::from_nanos((value * nanos_per_unit) as u64);
    }

    Ok(total)
}

/// Formats a duration with the largest exact unit, for example `250ms` or `30s`.
#[must_use]
pub fn format(duration: Duration) -> String {
    const UNITS: [(u128, &str); 6] = [
        (86_400_000_000_000, "d"),
        (3_600_000_000_000, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
    ];

    let nanos = duration.as_nanos();
    for (size, unit) in UNITS {
        if nanos >= size && nanos.is_multiple_of(size) {
            return format!("{}{unit}", nanos / size);
        }
    }

    format!("{nanos}ns")
}

/// Serializes a [`Duration`] as a human readable string.
pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format(*duration))
}

/// Deserializes a [`Duration`] from a string such as `5s` or from seconds.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Seconds(u64),
        Text(String),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Seconds(seconds) => Ok(Duration::from_secs(seconds)),
        Raw::Text(text) => parse(&text).map_err(de::Error::custom),
    }
}

/// Serde helpers for `Option<Duration>` fields.
pub mod option {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes `Some(duration)` as a string and `None` as nothing.
    pub fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match duration {
            Some(duration) => super::serialize(duration, serializer),
            None => serializer.serialize_none(),
        }
    }

    /// Deserializes a duration, wrapping it in `Some`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        #[derive(Deserialize)]
        struct Wrapper(#[serde(deserialize_with = "super::deserialize")] Duration);

        Ok(Some(Wrapper::deserialize(deserializer)?.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_units() {
        assert_eq!(parse("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(parse("5s"), Ok(Duration::from_secs(5)));
        assert_eq!(parse("1m30s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse("1.5s"), Ok(Duration::from_millis(1500)));
        assert_eq!(parse("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse("10"), Ok(Duration::from_secs(10)));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("").is_err());
        assert!(parse("5x").is_err());
        assert!(parse("ms").is_err());
    }

    #[test]
    fn format_round_trips() {
        for text in ["250ms", "5s", "2m", "3h", "1d", "1500ms"] {
            assert_eq!(format(parse(text).unwrap()), text);
        }
    }
}
