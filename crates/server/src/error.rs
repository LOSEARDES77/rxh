//! Top level error type.

use std::fmt;
use std::io;

use rxh_core::config::ConfigError;

/// Error returned by the public API of this crate.
#[derive(Debug)]
pub enum Error {
    /// Socket or file system error.
    Io(io::Error),

    /// Invalid configuration.
    Config(ConfigError),

    /// HTTP protocol error.
    Http(hyper::Error),
}

impl std::error::Error for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "IO error: {err}"),
            Error::Config(err) => write!(f, "configuration error: {err}"),
            Error::Http(err) => write!(f, "HTTP error: {err}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Error::Io(value)
    }
}

impl From<ConfigError> for Error {
    fn from(value: ConfigError) -> Self {
        Error::Config(value)
    }
}

impl From<hyper::Error> for Error {
    fn from(value: hyper::Error) -> Self {
        Error::Http(value)
    }
}
