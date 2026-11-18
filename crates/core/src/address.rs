//! Backend address forms: `host:port`, `http://host[:port]`, `https://host[:port]`
//! and `unix//absolute/path.sock` (Caddy syntax).

/// URL scheme written in front of a backend address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    /// No scheme: `host:port`.
    Bare,

    /// `http://`, default port 80.
    Http,

    /// `https://`, default port 443.
    Https,
}

/// A parsed backend address. Borrows from the string it was parsed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAddress<'a> {
    /// TCP endpoint.
    Tcp {
        /// Host name or IP; IPv6 literals keep their brackets.
        host: &'a str,

        /// Port, with the scheme default applied.
        port: u16,

        /// Scheme as written.
        scheme: Scheme,
    },

    /// Unix domain socket at this absolute path.
    Unix(&'a str),
}

impl<'a> BackendAddress<'a> {
    /// Parses one address. Port ranges must be expanded first.
    ///
    /// # Errors
    ///
    /// Returns a message when the address has none of the accepted forms.
    pub fn parse(address: &'a str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "invalid backend address {address:?}, expected host:port, http://host[:port], \
                 https://host[:port] or unix//absolute/path"
            )
        };

        if let Some(path) = address.strip_prefix("unix/") {
            return if path.len() > 1 && path.starts_with('/') {
                Ok(Self::Unix(path))
            } else {
                Err(invalid())
            };
        }

        let (scheme, rest) = split_scheme(address);
        let (host, port) = split_host_port(rest).ok_or_else(invalid)?;
        let port = match (port, scheme) {
            (Some(port), _) => port.parse().map_err(|_| invalid())?,
            (None, Scheme::Http) => 80,
            (None, Scheme::Https) => 443,
            (None, Scheme::Bare) => return Err(invalid()),
        };
        Ok(Self::Tcp { host, port, scheme })
    }

    /// `host:port` of a TCP address.
    #[must_use]
    pub fn host_port(&self) -> Option<String> {
        match self {
            Self::Tcp { host, port, .. } => Some(format!("{host}:{port}")),
            Self::Unix(_) => None,
        }
    }
}

/// Splits `rest` into the scheme and what follows it.
pub(crate) fn split_scheme(address: &str) -> (Scheme, &str) {
    if let Some(rest) = address.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = address.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        (Scheme::Bare, address)
    }
}

/// Splits `host[:port]`, understanding `[v6]:port`. The host is non-empty and
/// has no `/`.
pub(crate) fn split_host_port(text: &str) -> Option<(&str, Option<&str>)> {
    let (host, port) = if text.starts_with('[') {
        let end = text.find(']')?;
        let port = match &text[end + 1..] {
            "" => None,
            after => Some(after.strip_prefix(':')?),
        };
        (&text[..=end], port)
    } else {
        match text.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (text, None),
        }
    };
    (!host.is_empty() && !host.contains('/')).then_some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_form() {
        let tcp = |host, port, scheme| BackendAddress::Tcp { host, port, scheme };
        assert_eq!(
            BackendAddress::parse("a:80"),
            Ok(tcp("a", 80, Scheme::Bare))
        );
        assert_eq!(
            BackendAddress::parse("https://a"),
            Ok(tcp("a", 443, Scheme::Https))
        );
        assert_eq!(
            BackendAddress::parse("http://a"),
            Ok(tcp("a", 80, Scheme::Http))
        );
        assert_eq!(
            BackendAddress::parse("https://[::1]:8443"),
            Ok(tcp("[::1]", 8443, Scheme::Https))
        );
        assert_eq!(
            BackendAddress::parse("unix//run/app.sock"),
            Ok(BackendAddress::Unix("/run/app.sock"))
        );
    }

    #[test]
    fn rejects_bad_forms() {
        for bad in [
            "a",
            ":80",
            "https://",
            "http://a/x",
            "unix/rel",
            "unix//",
            "a:x",
        ] {
            assert!(BackendAddress::parse(bad).is_err(), "{bad}");
        }
    }
}
