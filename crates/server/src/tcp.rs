//! Layer 4 TCP proxy: picks a backend per connection and copies bytes both
//! ways. Optional TLS termination happens before this, in the listener.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use rxh_core::config::ProxyProtocol;
use rxh_core::pool::{SelectContext, Upstream};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::registry::Group;
use crate::upstream::{self, DialError};

/// Proxies one client connection to a backend of `group`.
pub(crate) async fn proxy<S>(
    mut client: S,
    peer: SocketAddr,
    local: SocketAddr,
    group: &Group,
    proxy_protocol: Option<ProxyProtocol>,
    log_name: &str,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let context = SelectContext::l4(peer.ip());
    let dial_timeout = group.pool.settings().dial_timeout;

    let dialed =
        upstream::connect_with_retries(
            &group.pool,
            &context,
            |upstream: Arc<Upstream>| async move {
                upstream::dial(upstream.address(), dial_timeout).await
            },
        )
        .await;

    let (selection, mut backend) = match dialed {
        Ok(dialed) => dialed,
        Err(DialError::NoUpstream) => {
            eprintln!("{log_name} => {peer}: no upstreams available");
            return;
        }
        Err(DialError::Connect(err)) => {
            eprintln!("{log_name} => {peer}: all dial attempts failed: {err}");
            return;
        }
    };

    let _guard = selection.upstream.start_request();
    let address = selection.upstream.address();

    if let Some(version) = proxy_protocol
        && let Err(err) = backend.write_all(&proxy_header(version, peer, local)).await
    {
        selection.upstream.record_failure();
        eprintln!("{log_name} => {peer}: PROXY header to {address} failed: {err}");
        return;
    }

    match tokio::io::copy_bidirectional(&mut client, &mut backend).await {
        Ok((sent, received)) => {
            println!("{peer} -> {log_name} TCP {address} sent {sent}B received {received}B");
        }
        Err(err) => eprintln!("{peer} -> {log_name} TCP {address} closed: {err}"),
    }
}

/// Builds a PROXY protocol header (HAProxy spec, versions 1 and 2).
pub(crate) fn proxy_header(
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
            const SIGNATURE: &[u8; 12] = b"\r\n\r\n\0\r\nQUIT\n";
            let mut header = SIGNATURE.to_vec();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_protocol_v1() {
        let header = proxy_header(
            ProxyProtocol::V1,
            "192.0.2.1:5000".parse().unwrap(),
            "198.51.100.1:443".parse().unwrap(),
        );
        assert_eq!(header, b"PROXY TCP4 192.0.2.1 198.51.100.1 5000 443\r\n");
    }

    #[test]
    fn proxy_protocol_v2() {
        let header = proxy_header(
            ProxyProtocol::V2,
            "192.0.2.1:5000".parse().unwrap(),
            "198.51.100.1:443".parse().unwrap(),
        );
        assert_eq!(&header[..12], b"\r\n\r\n\0\r\nQUIT\n");
        assert_eq!(header[12], 0x21);
        assert_eq!(header[13], 0x11);
        assert_eq!(u16::from_be_bytes([header[14], header[15]]), 12);
        assert_eq!(&header[16..20], &[192, 0, 2, 1]);
        assert_eq!(&header[24..26], &5000_u16.to_be_bytes());
        assert_eq!(header.len(), 28);
    }
}
