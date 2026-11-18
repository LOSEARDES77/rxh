//! Byte sizes such as `4096`, `64KiB` or `10MB`.
//!
//! Sizes in the configuration file are a number of bytes, or a string with a
//! unit like in Caddy: decimal (`KB`, `MB`, `GB`, powers of 1000) or binary
//! (`KiB`, `MiB`, `GiB`, powers of 1024). Units are case insensitive.

use std::fmt;

use serde::de;
use serde::{Deserialize, Deserializer};

/// Error returned when a size string cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSizeError(String);

impl fmt::Display for ParseSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid size {:?}, expected bytes or something like 64KiB or 10MB",
            self.0
        )
    }
}

impl std::error::Error for ParseSizeError {}

/// Parses a size like `10MB` into bytes. A plain number means bytes.
pub fn parse(input: &str) -> Result<usize, ParseSizeError> {
    let error = || ParseSizeError(input.to_owned());
    let text = input.trim();

    let number_len = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let value: usize = text[..number_len].parse().map_err(|_| error())?;
    let multiplier: usize = match text[number_len..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kb" => 1000,
        "mb" => 1000 * 1000,
        "gb" => 1000 * 1000 * 1000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        _ => return Err(error()),
    };
    value.checked_mul(multiplier).ok_or_else(error)
}

/// Deserializes a size from a number of bytes or a string such as `10MB`.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<usize, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Bytes(usize),
        Text(String),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Bytes(bytes) => Ok(bytes),
        Raw::Text(text) => parse(&text).map_err(de::Error::custom),
    }
}

/// Deserializes a size, wrapping it in `Some`.
pub fn deserialize_option<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<usize>, D::Error> {
    deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bytes_and_units() {
        assert_eq!(parse("0"), Ok(0));
        assert_eq!(parse("4096"), Ok(4096));
        assert_eq!(parse("512b"), Ok(512));
        assert_eq!(parse("10MB"), Ok(10_000_000));
        assert_eq!(parse("2kb"), Ok(2000));
        assert_eq!(parse("64KiB"), Ok(65_536));
        assert_eq!(parse(" 1 MiB "), Ok(1_048_576));
        assert_eq!(parse("1GiB"), Ok(1 << 30));
        assert_eq!(parse("1GB"), Ok(1_000_000_000));
    }

    #[test]
    fn rejects_garbage() {
        for text in [
            "",
            "MB",
            "-1",
            "1.5MB",
            "10XB",
            "10 M B",
            "99999999999999999999",
        ] {
            assert!(parse(text).is_err(), "{text:?} should be rejected");
        }
        assert!(parse(&format!("{}GiB", usize::MAX)).is_err(), "overflow");
    }
}
