//! IP ranges in CIDR notation, used for `trusted_proxies`, `remote_ip`,
//! `ip_allow` and `ip_deny`.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An IP network such as `10.0.0.0/8` or `::1/128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub struct IpNet {
    address: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Private, loopback and link-local ranges (Caddy's `private_ranges`).
    #[must_use]
    pub fn private_ranges() -> Vec<IpNet> {
        [
            "192.168.0.0/16",
            "172.16.0.0/12",
            "10.0.0.0/8",
            "127.0.0.1/8",
            "fd00::/8",
            "::1/128",
        ]
        .iter()
        .map(|range| range.parse().expect("built-in ranges are valid"))
        .collect()
    }

    /// Returns true if `ip` belongs to this network.
    #[must_use]
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.address, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// Returns true if `ip` passes the `ip_allow` and `ip_deny` lists: it is in
/// no `deny` range, and in an `allow` range unless `allow` is empty. A deny
/// wins over an allow, so `deny` can cut holes in the `allow` ranges.
#[must_use]
pub fn permitted(ip: IpAddr, allow: &[IpNet], deny: &[IpNet]) -> bool {
    !deny.iter().any(|net| net.contains(ip))
        && (allow.is_empty() || allow.iter().any(|net| net.contains(ip)))
}

impl FromStr for IpNet {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("invalid IP range {text:?}, expected CIDR like 10.0.0.0/8");
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (
                address.parse::<IpAddr>().map_err(|_| invalid())?,
                Some(prefix),
            ),
            None => (text.parse::<IpAddr>().map_err(|_| invalid())?, None),
        };

        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| invalid())?,
            None => max,
        };
        if prefix > max {
            return Err(invalid());
        }

        Ok(Self { address, prefix })
    }
}

impl TryFrom<String> for IpNet {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<IpNet> for String {
    fn from(value: IpNet) -> Self {
        value.to_string()
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains() {
        let net: IpNet = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains("10.20.30.40".parse().unwrap()));
        assert!(!net.contains("11.0.0.1".parse().unwrap()));
        assert!(net.contains("::ffff:10.1.1.1".parse().unwrap()));

        let all: IpNet = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("1.2.3.4".parse().unwrap()));

        let single: IpNet = "::1".parse().unwrap();
        assert!(single.contains("::1".parse().unwrap()));
        assert!(!single.contains("::2".parse().unwrap()));
    }

    #[test]
    fn permitted_checks_deny_before_allow() {
        let nets =
            |list: &[&str]| -> Vec<IpNet> { list.iter().map(|net| net.parse().unwrap()).collect() };
        let ip = |text: &str| -> IpAddr { text.parse().unwrap() };

        assert!(permitted(ip("1.2.3.4"), &[], &[]));

        let allow = nets(&["10.0.0.0/8", "::1"]);
        assert!(permitted(ip("10.1.2.3"), &allow, &[]));
        assert!(permitted(ip("::1"), &allow, &[]));
        assert!(!permitted(ip("11.0.0.1"), &allow, &[]));

        let deny = nets(&["10.0.0.0/24"]);
        assert!(!permitted(ip("10.0.0.7"), &allow, &deny));
        assert!(permitted(ip("10.0.1.7"), &allow, &deny));
        assert!(!permitted(ip("10.0.0.7"), &[], &deny));
        assert!(permitted(ip("192.168.1.1"), &[], &deny));
        assert!(!permitted(ip("::ffff:10.0.0.7"), &[], &deny));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!("10.0.0.0/33".parse::<IpNet>().is_err());
        assert!("nope".parse::<IpNet>().is_err());
    }
}
