//! PROXY protocol (HAProxy spec, versions 1 and 2): the header RXH sends to
//! backends, and the parser for the header clients send when a server
//! sets `proxy_protocol_in`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use rxh_core::config::ProxyProtocol;
use tokio::io::AsyncRead;

use crate::l4match;

const V2_SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";

/// Longest version 1 header, `\r\n` included.
const V1_MAX: usize = 107;

/// Longest version 2 header: 16 fixed bytes and a 16 bit length.
const V2_MAX: usize = 16 + u16::MAX as usize;

/// A parsed incoming header.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Header {
    /// Client address, or `None` to keep the real peer (`LOCAL`, `UNKNOWN`,
    /// unix or unspecified addresses).
    pub(crate) source: Option<SocketAddr>,

    /// Length of the header in bytes.
    pub(crate) len: usize,
}

/// Builds a header announcing a connection from `source` to `destination`.
pub(crate) fn header(
    version: ProxyProtocol,
    source: SocketAddr,
    destination: SocketAddr,
) -> Vec<u8> {
    let (source_ip, destination_ip) = (source.ip().to_canonical(), destination.ip().to_canonical());

    match version {
        ProxyProtocol::V1 => {
            let line = match (source_ip, destination_ip) {
                (IpAddr::V4(src), IpAddr::V4(dst)) => {
                    format!(
                        "PROXY TCP4 {src} {dst} {} {}\r\n",
                        source.port(),
                        destination.port()
                    )
                }
                (IpAddr::V6(src), IpAddr::V6(dst)) => {
                    format!(
                        "PROXY TCP6 {src} {dst} {} {}\r\n",
                        source.port(),
                        destination.port()
                    )
                }
                _ => String::from("PROXY UNKNOWN\r\n"),
            };
            line.into_bytes()
        }
        ProxyProtocol::V2 => {
            let mut header = V2_SIGNATURE.to_vec();
            // Version 2, PROXY command.
            header.push(0x21);

            match (source_ip, destination_ip) {
                (IpAddr::V4(src), IpAddr::V4(dst)) => {
                    header.push(0x11);
                    header.extend_from_slice(&12_u16.to_be_bytes());
                    header.extend_from_slice(&src.octets());
                    header.extend_from_slice(&dst.octets());
                }
                (IpAddr::V6(src), IpAddr::V6(dst)) => {
                    header.push(0x21);
                    header.extend_from_slice(&36_u16.to_be_bytes());
                    header.extend_from_slice(&src.octets());
                    header.extend_from_slice(&dst.octets());
                }
                _ => {
                    // Mixed families: UNSPEC, no address block.
                    header.push(0x00);
                    header.extend_from_slice(&0_u16.to_be_bytes());
                    return header;
                }
            }

            header.extend_from_slice(&source.port().to_be_bytes());
            header.extend_from_slice(&destination.port().to_be_bytes());
            header
        }
    }
}

/// Reads a version 1 or 2 header from `stream` within `limit`. Returns the
/// client address it carries (`None` keeps the real peer) and leaves the
/// bytes read after the header in `buffer`.
pub(crate) async fn read<S>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    limit: Duration,
) -> io::Result<Option<SocketAddr>>
where
    S: AsyncRead + Unpin,
{
    let parsed = l4match::read_until(stream, buffer, V2_MAX, limit, |bytes| {
        parse(bytes).transpose()
    })
    .await?;
    match parsed {
        None => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no complete PROXY header in time",
        )),
        Some(Err(err)) => Err(io::Error::new(io::ErrorKind::InvalidData, err)),
        Some(Ok(header)) => {
            buffer.drain(..header.len.min(buffer.len()));
            Ok(header.source)
        }
    }
}

/// Parses the header at the start of `bytes`. `Ok(None)` means more bytes
/// are needed.
pub(crate) fn parse(bytes: &[u8]) -> Result<Option<Header>, &'static str> {
    if bytes.starts_with(V2_SIGNATURE) {
        return parse_v2(bytes);
    }
    if bytes.starts_with(b"PROXY ") {
        return parse_v1(bytes);
    }
    if V2_SIGNATURE.starts_with(bytes) || b"PROXY ".starts_with(bytes) {
        return Ok(None);
    }
    Err("not a PROXY protocol header")
}

fn parse_v1(bytes: &[u8]) -> Result<Option<Header>, &'static str> {
    let window = bytes.get(..V1_MAX).unwrap_or(bytes);
    let Some(end) = window.windows(2).position(|pair| pair == b"\r\n") else {
        return if bytes.len() >= V1_MAX {
            Err("PROXY v1 header too long")
        } else {
            Ok(None)
        };
    };
    let line = window.get(..end).unwrap_or_default();
    let line = std::str::from_utf8(line).map_err(|_| "PROXY v1 header is not text")?;
    let fields: Vec<&str> = line.split(' ').collect();

    let source = match fields.as_slice() {
        ["PROXY", "UNKNOWN", ..] => None,
        [
            "PROXY",
            family @ ("TCP4" | "TCP6"),
            source,
            destination,
            source_port,
            destination_port,
        ] => {
            let v4 = *family == "TCP4";
            let address = |ip: &str, port: &str| -> Result<SocketAddr, &'static str> {
                let ip: IpAddr = ip.parse().map_err(|_| "invalid PROXY v1 address")?;
                if ip.is_ipv4() != v4 {
                    return Err("PROXY v1 address does not match its family");
                }
                let port: u16 = port.parse().map_err(|_| "invalid PROXY v1 port")?;
                Ok(SocketAddr::new(ip, port))
            };
            address(destination, destination_port)?;
            Some(address(source, source_port)?)
        }
        _ => return Err("malformed PROXY v1 header"),
    };
    Ok(Some(Header {
        source,
        len: end + 2,
    }))
}

fn parse_v2(bytes: &[u8]) -> Result<Option<Header>, &'static str> {
    let Some((&head, rest)) = bytes.split_first_chunk::<16>() else {
        return Ok(None);
    };
    let [.., version_command, family, high, low] = head;
    if version_command >> 4 != 2 {
        return Err("unsupported PROXY protocol version");
    }
    let len = usize::from(u16::from_be_bytes([high, low]));
    let Some(addresses) = rest.get(..len) else {
        return Ok(None);
    };

    let source = match version_command & 0x0f {
        // LOCAL: health checks of the proxy itself, keep the real peer.
        0x0 => None,
        0x1 => match family >> 4 {
            // UNSPEC and unix sockets: no usable client address.
            0x0 | 0x3 => None,
            0x1 => {
                let Some(&[a, b, c, d, _, _, _, _, high, low, _, _]) = addresses.first_chunk()
                else {
                    return Err("PROXY v2 IPv4 address block too short");
                };
                let ip = Ipv4Addr::new(a, b, c, d);
                Some(SocketAddr::new(ip.into(), u16::from_be_bytes([high, low])))
            }
            0x2 => {
                let Some((&ip, rest)) = addresses.split_first_chunk::<16>() else {
                    return Err("PROXY v2 IPv6 address block too short");
                };
                let Some(&[high, low]) = rest.get(16..).and_then(<[u8]>::first_chunk) else {
                    return Err("PROXY v2 IPv6 address block too short");
                };
                let ip = Ipv6Addr::from(ip);
                Some(SocketAddr::new(ip.into(), u16::from_be_bytes([high, low])))
            }
            _ => return Err("unsupported PROXY v2 address family"),
        },
        _ => return Err("unsupported PROXY v2 command"),
    };
    Ok(Some(Header {
        source,
        len: 16 + len,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn writes_v1() {
        let header = header(
            ProxyProtocol::V1,
            address("192.0.2.1:5000"),
            address("198.51.100.1:443"),
        );
        assert_eq!(header, b"PROXY TCP4 192.0.2.1 198.51.100.1 5000 443\r\n");
    }

    #[test]
    fn writes_v2() {
        let header = header(
            ProxyProtocol::V2,
            address("192.0.2.1:5000"),
            address("198.51.100.1:443"),
        );
        assert_eq!(&header[..12], b"\r\n\r\n\0\r\nQUIT\n");
        assert_eq!(header[12], 0x21);
        assert_eq!(header[13], 0x11);
        assert_eq!(u16::from_be_bytes([header[14], header[15]]), 12);
        assert_eq!(&header[16..20], &[192, 0, 2, 1]);
        assert_eq!(&header[24..26], &5000_u16.to_be_bytes());
        assert_eq!(header.len(), 28);
    }

    #[test]
    fn parses_what_it_writes() {
        for (source, destination) in [
            ("192.0.2.1:5000", "198.51.100.1:443"),
            ("[2001:db8::1]:5000", "[2001:db8::2]:443"),
        ] {
            for version in [ProxyProtocol::V1, ProxyProtocol::V2] {
                let mut bytes = header(version, address(source), address(destination));
                let len = bytes.len();
                bytes.extend_from_slice(b"GET / HTTP/1.1\r\n");
                assert_eq!(
                    parse(&bytes),
                    Ok(Some(Header {
                        source: Some(address(source)),
                        len
                    })),
                    "{version:?} {source}"
                );
            }
        }
    }

    #[test]
    fn v1_unknown_and_errors() {
        assert_eq!(
            parse(b"PROXY UNKNOWN\r\nrest"),
            Ok(Some(Header {
                source: None,
                len: 15
            }))
        );
        assert_eq!(
            parse(b"PROXY UNKNOWN ffff::1 ffff::2 1 2\r\n").map(|h| h.map(|h| h.source)),
            Ok(Some(None))
        );
        for bad in [
            &b"PROXY TCP4 1.2.3.4 5.6.7.8 1\r\n"[..],
            b"PROXY TCP4 ::1 ::2 1 2\r\n",
            b"PROXY TCP6 1.2.3.4 5.6.7.8 1 2\r\n",
            b"PROXY TCP4 1.2.3.4 5.6.7.8 1 70000\r\n",
            b"PROXY TCP4 1.2.3.4 5.6.7.8 x 2\r\n",
            b"PROXY UDP4 1.2.3.4 5.6.7.8 1 2\r\n",
            b"PROXY  TCP4 1.2.3.4 5.6.7.8 1 2\r\n",
            b"PROXY \xff\r\n",
            b"GET / HTTP/1.1\r\n",
            b"\x16\x03\x01",
        ] {
            assert!(parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let long = [&b"PROXY "[..], &[b'1'; 200]].concat();
        assert!(parse(&long).is_err());
    }

    #[test]
    fn v2_local_unspec_and_errors() {
        let mut local = V2_SIGNATURE.to_vec();
        local.extend_from_slice(&[0x20, 0x00, 0x00, 0x03, 1, 2, 3]);
        assert_eq!(
            parse(&local),
            Ok(Some(Header {
                source: None,
                len: 19
            }))
        );

        let with_family = |version_command: u8, family: u8, block: &[u8]| {
            let mut bytes = V2_SIGNATURE.to_vec();
            bytes.extend_from_slice(&[version_command, family]);
            bytes.extend_from_slice(&u16::try_from(block.len()).unwrap().to_be_bytes());
            bytes.extend_from_slice(block);
            bytes
        };
        assert_eq!(
            parse(&with_family(0x21, 0x00, &[])).map(|h| h.map(|h| h.source)),
            Ok(Some(None))
        );
        // TLVs after the addresses are skipped.
        let mut block = vec![10, 0, 0, 1, 10, 0, 0, 2, 0, 80, 1, 187];
        block.extend_from_slice(&[0x04, 0x00, 0x01, 0xaa]);
        assert_eq!(
            parse(&with_family(0x21, 0x11, &block)),
            Ok(Some(Header {
                source: Some(address("10.0.0.1:80")),
                len: 16 + 16
            }))
        );
        assert!(parse(&with_family(0x21, 0x11, &[1, 2, 3])).is_err());
        assert!(parse(&with_family(0x21, 0x21, &[0; 20])).is_err());
        assert!(parse(&with_family(0x31, 0x11, &[0; 12])).is_err());
        assert!(parse(&with_family(0x22, 0x11, &[0; 12])).is_err());
        assert!(parse(&with_family(0x21, 0x51, &[0; 12])).is_err());
    }

    #[test]
    fn truncated_input_asks_for_more() {
        let headers = [
            header(
                ProxyProtocol::V1,
                address("192.0.2.1:5000"),
                address("198.51.100.1:443"),
            ),
            header(
                ProxyProtocol::V2,
                address("[2001:db8::1]:5000"),
                address("[2001:db8::2]:443"),
            ),
        ];
        for full in headers {
            for end in 0..full.len() {
                assert_eq!(parse(&full[..end]), Ok(None), "prefix of {end} bytes");
            }
        }
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        for len in 0..600 {
            let mut bytes: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed.to_le_bytes()[0]
                })
                .collect();
            let _ = parse(&bytes);
            // Valid prefixes followed by garbage.
            bytes.splice(0..0, V2_SIGNATURE.iter().copied());
            let _ = parse(&bytes);
            bytes.splice(0..12, b"PROXY TCP4 ".iter().copied());
            let _ = parse(&bytes);
        }
    }
}
