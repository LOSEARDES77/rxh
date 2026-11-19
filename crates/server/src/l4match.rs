//! Layer 4 matching: looks at the client address and the first bytes of a
//! connection (TLS ClientHello, HTTP/1 request head, SSH banner, PostgreSQL
//! startup message, or a regular expression) to pick a `tcp` route.
//!
//! The bytes read while sniffing are not lost: [`Rewind`] replays them to
//! whoever reads the connection next. Everything here parses untrusted input,
//! so every read is bounded and every parser fails closed instead of panicking.

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use regex::bytes::Regex;
use rxh_core::config::{L4Protocol, Pattern};
use rxh_core::matcher::HostPattern;
use rxh_core::net::IpNet;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};

/// Most bytes read from a client before matching gives up on more.
const MAX_SNIFF: usize = 16 * 1024;

/// What the first bytes of a connection (or its TLS handshake) revealed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Detected {
    /// A TLS ClientHello, or a connection whose TLS the server terminated.
    pub(crate) tls: bool,

    /// An HTTP/1 request line or the HTTP/2 preface.
    pub(crate) http: bool,

    /// An SSH banner.
    pub(crate) ssh: bool,

    /// A PostgreSQL startup message or SSL, GSSAPI or cancel request.
    pub(crate) postgres: bool,

    /// TLS server name.
    pub(crate) sni: Option<String>,

    /// ALPN protocols offered by the client, or the one negotiated when the
    /// server terminates TLS.
    pub(crate) alpn: Vec<Vec<u8>>,

    /// `Host` header of an HTTP/1 request.
    pub(crate) http_host: Option<String>,
}

/// Compiled layer 4 matchers of one `match` entry and of the entries whose
/// `subroute` holds it. Every given matcher must hold; inside a list any item
/// may match. No matchers match everything.
#[derive(Debug)]
pub(crate) struct L4Matcher {
    levels: Vec<Level>,
}

/// Matchers of a single entry.
#[derive(Debug)]
struct Level {
    remote_ip: Vec<IpNet>,
    sni: Vec<HostPattern>,
    alpn: Vec<Vec<u8>>,
    protocol: Option<L4Protocol>,
    http_host: Vec<HostPattern>,
    regexp: Option<(Regex, usize)>,
}

impl L4Matcher {
    /// Compiles the layer 4 matchers of `chain`: an entry preceded by the
    /// entries it is nested in, outermost first.
    pub(crate) fn new(chain: &[&Pattern]) -> Result<Self, String> {
        let hosts = |hosts: &[String]| -> Result<Vec<HostPattern>, String> {
            hosts.iter().map(|host| HostPattern::new(host)).collect()
        };
        let mut levels = Vec::with_capacity(chain.len());
        for pattern in chain {
            let regexp = pattern.compile_regexp()?;
            levels.push(Level {
                remote_ip: pattern.remote_ip.clone(),
                sni: hosts(&pattern.sni)?,
                alpn: pattern
                    .alpn
                    .iter()
                    .map(|alpn| alpn.as_bytes().to_vec())
                    .collect(),
                protocol: pattern.protocol,
                http_host: hosts(&pattern.http_host)?,
                regexp: regexp.map(|regexp| (regexp, pattern.regexp_count())),
            });
        }
        Ok(Self { levels })
    }

    /// Returns true if the entry matches every connection.
    pub(crate) fn is_default(&self) -> bool {
        self.levels.iter().all(|level| {
            level.remote_ip.is_empty()
                && level.sni.is_empty()
                && level.alpn.is_empty()
                && level.protocol.is_none()
                && level.http_host.is_empty()
                && level.regexp.is_none()
        })
    }

    /// Returns true if deciding needs the client's first bytes. When the
    /// server `terminated` TLS, `sni`, `alpn` and `protocol = "tls"` come from
    /// the handshake instead; `remote_ip` never needs them.
    pub(crate) fn needs_bytes(&self, terminated: bool) -> bool {
        self.levels.iter().any(|level| {
            let stream = !level.http_host.is_empty()
                || level.regexp.is_some()
                || !matches!(level.protocol, None | Some(L4Protocol::Tls));
            let hello = !level.sni.is_empty()
                || !level.alpn.is_empty()
                || level.protocol == Some(L4Protocol::Tls);
            stream || (hello && !terminated)
        })
    }

    /// Bytes the `regexp` matchers want to look at; 0 without any.
    pub(crate) fn regexp_count(&self) -> usize {
        let counts = self.levels.iter().filter_map(|level| level.regexp.as_ref());
        counts.map(|(_, count)| *count).max().unwrap_or(0)
    }

    /// Returns true if a connection from `peer` that started with `bytes`,
    /// in which `detected` was found, satisfies every matcher.
    pub(crate) fn matches(&self, detected: &Detected, peer: IpAddr, bytes: &[u8]) -> bool {
        let host = |patterns: &[HostPattern], name: Option<&str>| {
            patterns.is_empty()
                || name.is_some_and(|name| patterns.iter().any(|pattern| pattern.matches(name)))
        };
        self.levels.iter().all(|level| {
            let protocol = match level.protocol {
                None => true,
                Some(L4Protocol::Tls) => detected.tls,
                Some(L4Protocol::Http) => detected.http,
                Some(L4Protocol::Ssh) => detected.ssh,
                Some(L4Protocol::Postgres) => detected.postgres,
            };
            // A client that sent fewer bytes than `regexp_count` does not match.
            let regexp = level.regexp.as_ref().is_none_or(|(regexp, count)| {
                bytes
                    .get(..*count)
                    .is_some_and(|head| regexp.is_match(head))
            });
            protocol
                && regexp
                && (level.remote_ip.is_empty() || level.remote_ip.iter().any(|n| n.contains(peer)))
                && host(&level.sni, detected.sni.as_deref())
                && host(&level.http_host, detected.http_host.as_deref())
                && (level.alpn.is_empty() || detected.alpn.iter().any(|a| level.alpn.contains(a)))
        })
    }
}

/// Reads the first bytes of `stream` into `buffer` (at most 16 KiB, for at
/// most `limit`) until they identify the protocol and there are at least
/// `want` of them (what the `regexp` matchers look at). A client that stays
/// silent, closes early or sends something unknown gets an empty
/// [`Detected`], which only matches entries without protocol matchers.
pub(crate) async fn sniff<S>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    limit: Duration,
    want: usize,
) -> io::Result<Detected>
where
    S: AsyncRead + Unpin,
{
    let enough = |bytes: &[u8]| detect(bytes).filter(|_| bytes.len() >= want);
    let found = read_until(stream, buffer, MAX_SNIFF, limit, enough).await?;
    // Fewer than `want` bytes may still identify the protocol.
    Ok(found.or_else(|| detect(buffer)).unwrap_or_default())
}

/// Reads from `stream` into `buffer` until `decide` returns a value. Gives
/// up with `None` when the buffer holds `max` bytes, the stream ends, or
/// `limit` passes; the bytes read so far stay in `buffer`.
pub(crate) async fn read_until<S, T>(
    stream: &mut S,
    buffer: &mut Vec<u8>,
    max: usize,
    limit: Duration,
    mut decide: impl FnMut(&[u8]) -> Option<T>,
) -> io::Result<Option<T>>
where
    S: AsyncRead + Unpin,
{
    let read = async {
        let mut chunk = [0; 4096];
        loop {
            // ponytail: `decide` reparses the whole buffer after every read, so
            // a client trickling bytes costs O(n²) work, bounded by `max`
            // (MAX_SNIFF, 16 KiB) and `limit` (match_timeout); keep parser
            // state across reads if that ever shows up in profiles.
            if let Some(found) = decide(buffer) {
                return Ok(Some(found));
            }
            let room = max.saturating_sub(buffer.len()).min(chunk.len());
            if room == 0 {
                return Ok(None);
            }
            let read = stream.read(&mut chunk[..room]).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
    };
    tokio::time::timeout(limit, read).await.unwrap_or(Ok(None))
}

/// Outcome of one detector on the bytes read so far.
enum Step<T> {
    /// More bytes could still match.
    More,
    /// Not this protocol.
    No,
    /// This protocol.
    Yes(T),
}

/// Identifies the protocol of `bytes`, or `None` if more bytes are needed.
pub(crate) fn detect(bytes: &[u8]) -> Option<Detected> {
    if bytes.is_empty() {
        return None;
    }
    match client_hello(bytes) {
        Step::Yes(hello) => {
            return Some(Detected {
                tls: true,
                sni: hello.sni,
                alpn: hello.alpn,
                ..Detected::default()
            });
        }
        Step::More => return None,
        Step::No => {}
    }
    if bytes.starts_with(b"SSH-") {
        return Some(Detected {
            ssh: true,
            ..Detected::default()
        });
    }
    if b"SSH-".starts_with(bytes) {
        return None;
    }
    match postgres(bytes) {
        Step::Yes(()) => {
            return Some(Detected {
                postgres: true,
                ..Detected::default()
            });
        }
        Step::More => return None,
        Step::No => {}
    }
    match http_request(bytes) {
        Step::Yes(host) => Some(Detected {
            http: true,
            http_host: host,
            ..Detected::default()
        }),
        Step::More => None,
        Step::No => Some(Detected::default()),
    }
}

/// SNI and ALPN of a ClientHello.
#[derive(Debug, Default, PartialEq, Eq)]
struct Hello {
    sni: Option<String>,
    alpn: Vec<Vec<u8>>,
}

/// Parses a ClientHello split over one or more TLS handshake records.
fn client_hello(bytes: &[u8]) -> Step<Hello> {
    const HANDSHAKE: u8 = 0x16;
    const CLIENT_HELLO: u8 = 0x01;

    let mut message = Vec::new();
    let mut rest = bytes;
    loop {
        // Record header: type, version (major 3), length.
        let Some((&[kind, major, _, high, low], fragment)) = rest.split_first_chunk::<5>() else {
            let partial_ok = rest.first().is_none_or(|&kind| kind == HANDSHAKE)
                && rest.get(1).is_none_or(|&major| major == 3);
            return if partial_ok { Step::More } else { Step::No };
        };
        let length = usize::from(u16::from_be_bytes([high, low]));
        if kind != HANDSHAKE || major != 3 || length == 0 || length > 16 * 1024 {
            return Step::No;
        }
        let Some((fragment, after)) = fragment.split_at_checked(length) else {
            return Step::More;
        };
        message.extend_from_slice(fragment);
        rest = after;

        // Handshake header: type and 24 bit length.
        let Some((&[kind, a, b, c], body)) = message.split_first_chunk::<4>() else {
            continue;
        };
        let length = usize::from(a) << 16 | usize::from(b) << 8 | usize::from(c);
        if kind != CLIENT_HELLO || length > MAX_SNIFF {
            return Step::No;
        }
        if let Some(body) = body.get(..length) {
            return parse_client_hello(body).map_or(Step::No, Step::Yes);
        }
    }
}

/// Bounds checked big endian reader over a byte slice.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, tail) = self.0.split_at_checked(len)?;
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        let (&first, tail) = self.0.split_first()?;
        self.0 = tail;
        Some(first)
    }

    fn u16(&mut self) -> Option<u16> {
        let (&pair, tail) = self.0.split_first_chunk::<2>()?;
        self.0 = tail;
        Some(u16::from_be_bytes(pair))
    }

    /// Vector with a one byte length prefix.
    fn vec8(&mut self) -> Option<Reader<'a>> {
        let len = self.u8()?;
        self.take(len.into()).map(Reader)
    }

    /// Vector with a two byte length prefix.
    fn vec16(&mut self) -> Option<Reader<'a>> {
        let len = self.u16()?;
        self.take(len.into()).map(Reader)
    }
}

/// Parses the body of a ClientHello handshake message (RFC 8446 4.1.2).
fn parse_client_hello(body: &[u8]) -> Option<Hello> {
    const SERVER_NAME: u16 = 0;
    const ALPN: u16 = 16;

    let mut reader = Reader(body);
    reader.take(2 + 32)?; // legacy_version, random
    reader.vec8()?; // legacy_session_id
    reader.vec16()?; // cipher_suites
    reader.vec8()?; // legacy_compression_methods

    let mut hello = Hello::default();
    if reader.is_empty() {
        // Old clients may omit the extensions entirely.
        return Some(hello);
    }
    let mut extensions = reader.vec16()?;
    while !extensions.is_empty() {
        let kind = extensions.u16()?;
        let mut data = extensions.vec16()?;
        match kind {
            SERVER_NAME => {
                let mut names = data.vec16()?;
                while !names.is_empty() {
                    let name_type = names.u8()?;
                    let name = names.vec16()?;
                    // Type 0 is host_name, the only one defined.
                    if name_type == 0 && hello.sni.is_none() {
                        hello.sni = Some(std::str::from_utf8(name.0).ok()?.to_owned());
                    }
                }
            }
            ALPN => {
                let mut protocols = data.vec16()?;
                while !protocols.is_empty() {
                    hello.alpn.push(protocols.vec8()?.0.to_vec());
                }
            }
            _ => {}
        }
    }
    Some(hello)
}

/// Recognizes the first message of a PostgreSQL client: a 32 bit length and
/// a 32 bit code, which is protocol version 3.x of a startup message or one
/// of the SSL, GSSAPI encryption and cancel requests.
fn postgres(bytes: &[u8]) -> Step<()> {
    const CANCEL: u32 = 80_877_102;
    const SSL: u32 = 80_877_103;
    const GSSENC: u32 = 80_877_104;
    /// Largest startup message PostgreSQL accepts.
    const MAX_STARTUP: u32 = 10_000;

    let Some((&[a, b, c, d, e, f, g, h], _)) = bytes.split_first_chunk::<8>() else {
        // Lengths fit in 16 bits: the message starts with two zero bytes.
        let plausible = bytes.iter().take(2).all(|&byte| byte == 0);
        return if plausible { Step::More } else { Step::No };
    };
    let length = u32::from_be_bytes([a, b, c, d]);
    let known = match u32::from_be_bytes([e, f, g, h]) {
        SSL | GSSENC => length == 8,
        CANCEL => length >= 16,
        version => version >> 16 == 3 && length >= 8,
    };
    if known && length <= MAX_STARTUP {
        Step::Yes(())
    } else {
        Step::No
    }
}

/// Recognizes an HTTP/1 request line, then looks for the `Host` header.
fn http_request(bytes: &[u8]) -> Step<Option<String>> {
    let Some(line_end) = find_crlf(bytes) else {
        // Wait for the rest of the request line only if it starts like one.
        let method = bytes.split(|&b| b == b' ').next().unwrap_or_default();
        let plausible = method.len() <= 16 && method.iter().all(u8::is_ascii_uppercase);
        return if plausible { Step::More } else { Step::No };
    };
    let line = bytes.get(..line_end).unwrap_or_default();
    let mut parts = line.split(|&b| b == b' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Step::No;
    };
    if method.is_empty() || !method.iter().all(u8::is_ascii_uppercase) || target.is_empty() {
        return Step::No;
    }
    match version {
        b"HTTP/1.0" | b"HTTP/1.1" => {}
        // Prior knowledge h2c: `PRI * HTTP/2.0`, no headers to look at.
        b"HTTP/2.0" => return Step::Yes(None),
        _ => return Step::No,
    }

    let mut rest = bytes.get(line_end + 2..).unwrap_or_default();
    loop {
        let Some(end) = find_crlf(rest) else {
            return Step::More;
        };
        let header = rest.get(..end).unwrap_or_default();
        if header.is_empty() {
            return Step::Yes(None);
        }
        if let Some((name, value)) = header.split_at_checked(5)
            && name.eq_ignore_ascii_case(b"host:")
        {
            let host = std::str::from_utf8(value).ok().map(str::trim);
            return Step::Yes(host.filter(|host| !host.is_empty()).map(str::to_owned));
        }
        rest = rest.get(end + 2..).unwrap_or_default();
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|pair| pair == b"\r\n")
}

/// A stream whose first reads return `prefix`, the bytes consumed while
/// sniffing or parsing a PROXY header. Writes go straight to the stream.
#[derive(Debug)]
pub(crate) struct Rewind<S> {
    prefix: Bytes,
    inner: S,
}

impl<S> Rewind<S> {
    pub(crate) fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: Bytes::from(prefix),
            inner,
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn get_ref(&self) -> &S {
        &self.inner
    }

    /// True while reads return the prefix.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn has_prefix(&self) -> bool {
        !self.prefix.is_empty()
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Rewind<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.prefix.is_empty() {
            return Pin::new(&mut this.inner).poll_read(cx, buf);
        }
        let len = this.prefix.len().min(buf.remaining());
        buf.put_slice(&this.prefix.split_to(len));
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Rewind<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rxh_core::config::Config;
    use tokio_rustls::rustls::pki_types::ServerName;
    use tokio_rustls::rustls::{ClientConfig, ClientConnection, RootCertStore};

    use super::*;

    /// First flight of a real rustls client: one TLS record with a ClientHello.
    fn rustls_client_hello(server_name: &'static str) -> Vec<u8> {
        let mut config = ClientConfig::builder()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let name = ServerName::try_from(server_name).unwrap();
        let mut connection = ClientConnection::new(Arc::new(config), name).unwrap();
        let mut bytes = Vec::new();
        connection.write_tls(&mut bytes).unwrap();
        bytes
    }

    fn tls(sni: &str) -> Detected {
        Detected {
            tls: true,
            sni: Some(sni.to_owned()),
            alpn: vec![b"h2".to_vec(), b"http/1.1".to_vec()],
            ..Detected::default()
        }
    }

    #[test]
    fn real_client_hello() {
        let hello = rustls_client_hello("a.test");
        assert_eq!(hello[0], 0x16, "a handshake record");
        assert_eq!(detect(&hello), Some(tls("a.test")));
        // Application data after the hello does not matter.
        let mut longer = hello.clone();
        longer.extend_from_slice(b"\x17\x03\x03\x00\x01x");
        assert_eq!(detect(&longer), Some(tls("a.test")));
    }

    #[test]
    fn client_hello_split_over_records() {
        let hello = rustls_client_hello("b.test");
        let message = &hello[5..];
        let (first, second) = message.split_at(40);
        let mut records = Vec::new();
        for fragment in [first, second] {
            records.extend_from_slice(&[0x16, 0x03, 0x01]);
            records.extend_from_slice(&u16::try_from(fragment.len()).unwrap().to_be_bytes());
            records.extend_from_slice(fragment);
        }
        assert_eq!(detect(&records), Some(tls("b.test")));
    }

    #[test]
    fn truncated_client_hello_asks_for_more() {
        let hello = rustls_client_hello("a.test");
        for end in 0..hello.len() {
            assert_eq!(detect(&hello[..end]), None, "prefix of {end} bytes");
        }
    }

    #[test]
    fn garbage_never_panics() {
        let hello = rustls_client_hello("a.test");
        // Every single byte corrupted, then every length cut short.
        for index in 0..hello.len() {
            for value in [0x00, 0x01, 0x7f, 0xff] {
                let mut bytes = hello.clone();
                bytes[index] = value;
                let _ = detect(&bytes);
                let _ = detect(&bytes[..index]);
            }
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        for len in 0..2000 {
            let mut bytes: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed.to_le_bytes()[0]
                })
                .collect();
            let _ = detect(&bytes);
            bytes.splice(0..0, [0x16, 0x03, 0x01, 0x01, 0x00, 0x01, 0x00, 0x00, 0xfc]);
            let _ = detect(&bytes);
            bytes.splice(0..0, *b"GET / HTTP/1.1\r\n");
            let _ = detect(&bytes);
        }
        // Lengths that point past the end are rejected, not trusted.
        assert!(matches!(
            client_hello(b"\x16\x03\x01\x00\x08\x01\x00\x00\x04\x03\x03\xff\xff"),
            Step::No
        ));
    }

    #[test]
    fn http_ssh_and_unknown() {
        let http = |host: Option<&str>| Detected {
            http: true,
            http_host: host.map(str::to_owned),
            ..Detected::default()
        };
        assert_eq!(
            detect(b"GET / HTTP/1.1\r\nAccept: */*\r\nhost:  A.test:8080 \r\n"),
            Some(http(Some("A.test:8080")))
        );
        assert_eq!(detect(b"GET / HTTP/1.0\r\n\r\n"), Some(http(None)));
        assert_eq!(
            detect(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"),
            Some(http(None))
        );
        assert_eq!(detect(b"GET /pa"), None);
        assert_eq!(detect(b"GET / HTTP/1.1\r\nAccept: */*\r\n"), None);
        assert_eq!(
            detect(b"SSH-2.0-OpenSSH_9.6\r\n"),
            Some(Detected {
                ssh: true,
                ..Detected::default()
            })
        );
        assert_eq!(detect(b"SS"), None);
        for unknown in [
            &b"hello world\r\n"[..],
            b"get / HTTP/1.1\r\n",
            b"GET / SIP/2.0\r\n",
            b"GET  HTTP/1.1\r\n",
            b"\x00\x00\x00\x08\x04\xd2\x16\x2e",
        ] {
            assert_eq!(detect(unknown), Some(Detected::default()), "{unknown:?}");
        }
    }

    #[test]
    fn postgres_first_messages() {
        let postgres = Some(Detected {
            postgres: true,
            ..Detected::default()
        });
        let startup = b"\x00\x00\x00\x13\x00\x03\x00\x00user\0rxh\0\0";
        for message in [
            &startup[..],
            // Protocol 3.2, only the first 8 bytes read so far.
            b"\x00\x00\x00\x13\x00\x03\x00\x02",
            // SSLRequest, GSSENCRequest and CancelRequest.
            b"\x00\x00\x00\x08\x04\xd2\x16\x2f",
            b"\x00\x00\x00\x08\x04\xd2\x16\x30",
            b"\x00\x00\x00\x10\x04\xd2\x16\x2e\x00\x00\x00\x01\x00\x00\x00\x02",
        ] {
            assert_eq!(detect(message), postgres, "{message:?}");
        }
        for end in 0..8 {
            assert_eq!(detect(&startup[..end]), None, "prefix of {end} bytes");
        }
        for other in [
            // Protocol 2, an SSLRequest with a body, a length under 8 and
            // one over what PostgreSQL accepts.
            &b"\x00\x00\x00\x13\x00\x02\x00\x00"[..],
            b"\x00\x00\x00\x09\x04\xd2\x16\x2f",
            b"\x00\x00\x00\x04\x00\x03\x00\x00",
            b"\x00\x00\x27\x11\x00\x03\x00\x00",
            b"\x00\x01\x00\x00\x00\x03\x00\x00",
        ] {
            assert_eq!(detect(other), Some(Detected::default()), "{other:?}");
        }
    }

    #[test]
    fn matchers() {
        let config = Config::from_toml(
            r#"
            [[server]]
            listen = "127.0.0.1:1"
            protocol = "tcp"
            match = [
                { sni = "*.a.test", alpn = ["h2", "acme-tls/1"], forward = "x:1" },
                { protocol = "ssh", forward = "x:1" },
                { protocol = "http", http_host = "b.test", forward = "x:1" },
                { protocol = "tls", forward = "x:1" },
                { forward = "x:1" },
                { protocol = "postgres", forward = "x:1" },
                { remote_ip = ["10.0.0.0/8", "::1"], forward = "x:1" },
                { regexp = '^\x05[\x01-\xff]', regexp_count = 2, forward = "x:1" },
                { regexp = "ell", forward = "x:1" },
            ]
            "#,
        )
        .unwrap();
        let patterns = &config.servers[0].patterns;
        let matchers: Vec<L4Matcher> = patterns
            .iter()
            .map(|pattern| L4Matcher::new(&[pattern]).unwrap())
            .collect();
        let peer = IpAddr::from([192, 0, 2, 1]);
        let first =
            |detected: &Detected| matchers.iter().position(|m| m.matches(detected, peer, b""));

        let postgres = Detected {
            postgres: true,
            ..Detected::default()
        };
        assert!(matchers[5].matches(&postgres, peer, b""));
        assert!(!matchers[5].matches(&Detected::default(), peer, b""));

        let unknown = Detected::default();
        let from = |ip: &str| matchers[6].matches(&unknown, ip.parse().unwrap(), b"");
        assert!(from("10.1.2.3"));
        assert!(from("::1"));
        assert!(from("::ffff:10.1.2.3"), "IPv4 mapped addresses are IPv4");
        assert!(!from("192.0.2.1"));
        assert!(!matchers[6].is_default());
        assert!(!matchers[6].needs_bytes(false), "the address is enough");

        // Raw bytes, not UTF-8; only the first `regexp_count` bytes count.
        let socks = |bytes: &[u8]| matchers[7].matches(&unknown, peer, bytes);
        assert!(socks(b"\x05\x01\x00"));
        assert!(socks(b"\x05\xff"));
        assert!(!socks(b"\x05\x00\x05\x01"));
        assert!(!socks(b"\x05"), "fewer bytes than regexp_count");
        assert!(matchers[8].matches(&unknown, peer, b"hello"), "unanchored");
        assert!(!matchers[8].matches(&unknown, peer, b"ell"), "default of 4");
        assert!(!matchers[8].matches(&unknown, peer, b"shell"));
        assert!(matchers[7].needs_bytes(true));
        assert_eq!(matchers[7].regexp_count(), 2);
        assert_eq!(matchers[8].regexp_count(), 4);
        assert_eq!(matchers[0].regexp_count(), 0);

        // Nested in a subroute: the outer and the inner matchers must hold.
        let nested = L4Matcher::new(&[&patterns[6], &patterns[5], &patterns[8]]).unwrap();
        let inside = IpAddr::from([10, 0, 0, 1]);
        assert!(nested.matches(&postgres, inside, b"hello"));
        assert!(!nested.matches(&postgres, peer, b"hello"));
        assert!(!nested.matches(&unknown, inside, b"hello"));
        assert!(!nested.matches(&postgres, inside, b"world"));
        assert!(nested.needs_bytes(true));
        assert_eq!(nested.regexp_count(), 4);
        assert!(
            L4Matcher::new(&[&patterns[4], &patterns[4]])
                .unwrap()
                .is_default()
        );

        assert_eq!(first(&tls("www.a.test")), Some(0));
        assert_eq!(first(&tls("a.test")), Some(3), "wildcards need a label");
        let mut no_alpn = tls("www.a.test");
        no_alpn.alpn.clear();
        assert_eq!(first(&no_alpn), Some(3));
        let ssh = Detected {
            ssh: true,
            ..Detected::default()
        };
        assert_eq!(first(&ssh), Some(1));
        let http = |host: &str| Detected {
            http: true,
            http_host: Some(host.to_owned()),
            ..Detected::default()
        };
        assert_eq!(first(&http("B.test:80")), Some(2));
        assert_eq!(first(&http("c.test")), Some(4));
        assert_eq!(first(&Detected::default()), Some(4));

        assert!(matchers[4].is_default());
        assert!(matchers[0].needs_bytes(false));
        assert!(
            !matchers[0].needs_bytes(true),
            "sni comes from the handshake"
        );
        assert!(!matchers[3].needs_bytes(true));
        assert!(matchers[2].needs_bytes(true));
        assert!(!matchers[4].needs_bytes(false));
    }

    #[tokio::test]
    async fn sniff_replays_and_gives_up_in_time() {
        let (mut client, mut server) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut client, b"SSH-2.0-x\r\nrest")
            .await
            .unwrap();
        let mut buffer = Vec::new();
        let detected = sniff(&mut server, &mut buffer, Duration::from_secs(5), 15)
            .await
            .unwrap();
        assert!(detected.ssh);
        let mut replayed = Rewind::new(buffer, server);
        let mut text = vec![0; 15];
        replayed.read_exact(&mut text).await.unwrap();
        assert_eq!(text, b"SSH-2.0-x\r\nrest");

        // A silent client gets nothing detected once the limit passes.
        let (_client, mut silent) = tokio::io::duplex(64);
        let mut buffer = Vec::new();
        let detected = sniff(&mut silent, &mut buffer, Duration::from_millis(20), 0)
            .await
            .unwrap();
        assert_eq!(detected, Detected::default());

        // Fewer bytes than the regexp matchers want: the wait ends with the
        // limit and the protocol is still known.
        let (mut client, mut short) = tokio::io::duplex(64);
        tokio::io::AsyncWriteExt::write_all(&mut client, b"SSH-2.0-x\r\n")
            .await
            .unwrap();
        let mut buffer = Vec::new();
        let detected = sniff(&mut short, &mut buffer, Duration::from_millis(20), 64)
            .await
            .unwrap();
        assert!(detected.ssh);
        assert_eq!(buffer, b"SSH-2.0-x\r\n");
    }
}
