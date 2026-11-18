//! PROXY protocol: the parser never panics, never waits for more bytes than
//! the longest header, and reads back every header RXH writes.

#![no_main]

use std::net::SocketAddr;

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use rxh_core::config::ProxyProtocol;
use rxh_server::fuzzing;

/// Longest version 2 header.
const V2_MAX: usize = 16 + u16::MAX as usize;

#[derive(Arbitrary, Debug)]
enum Input<'a> {
    Parse(&'a [u8]),
    RoundTrip {
        v2: bool,
        source: SocketAddr,
        destination: SocketAddr,
        trailer: &'a [u8],
    },
}

fuzz_target!(|input: Input<'_>| {
    match input {
        Input::Parse(bytes) => parse(bytes),
        Input::RoundTrip {
            v2,
            source,
            destination,
            trailer,
        } => round_trip(v2, source, destination, trailer),
    }
});

fn parse(bytes: &[u8]) {
    match fuzzing::proxy_protocol_parse(bytes) {
        Ok(None) => assert!(bytes.len() < V2_MAX, "waiting must stay bounded"),
        Ok(Some((source, len))) => {
            assert!(len <= bytes.len(), "a header cannot outgrow its input");
            assert_eq!(
                fuzzing::proxy_protocol_parse(&bytes[..len]),
                Ok(Some((source, len))),
                "bytes after the header must not change it"
            );
            assert_eq!(
                fuzzing::proxy_protocol_parse(&bytes[..len - 1]),
                Ok(None),
                "a truncated header must wait for more bytes"
            );
        }
        Err(_) => {}
    }
}

fn round_trip(v2: bool, source: SocketAddr, destination: SocketAddr, trailer: &[u8]) {
    let version = if v2 {
        ProxyProtocol::V2
    } else {
        ProxyProtocol::V1
    };
    let mut bytes = fuzzing::proxy_protocol_header(version, source, destination);
    let len = bytes.len();
    bytes.extend_from_slice(trailer);

    let (source_ip, destination_ip) = (source.ip().to_canonical(), destination.ip().to_canonical());
    let expected = (source_ip.is_ipv4() == destination_ip.is_ipv4())
        .then(|| SocketAddr::new(source_ip, source.port()));
    assert_eq!(
        fuzzing::proxy_protocol_parse(&bytes),
        Ok(Some((expected, len))),
        "the parser must read back the header RXH writes"
    );
}
