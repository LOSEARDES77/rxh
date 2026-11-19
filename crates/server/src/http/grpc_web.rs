//! gRPC-Web translation: browsers cannot read HTTP trailers, so gRPC-Web
//! clients send `application/grpc-web` (or base64 `application/grpc-web-text`)
//! and expect the trailers as a last body frame flagged `0x80`. Requests are
//! turned into gRPC for the backend and responses back.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::{BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body_util::BodyExt;
use hyper::Response;
use hyper::body::Frame;
use hyper::header::{self, HeaderValue};

use crate::http::body::{Body, BodyError};

const GRPC: &str = "application/grpc";
const GRPC_WEB: &str = "application/grpc-web";
const TEXT: &str = "-text";

/// Body encoding of a gRPC-Web exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    /// `application/grpc-web`: gRPC frames as they are.
    Binary,
    /// `application/grpc-web-text`: the same, in base64.
    Text,
}

/// Turns the headers of a gRPC-Web request into those of a gRPC request.
/// `None`, with the headers untouched, for any other request.
pub(crate) fn translate_request(headers: &mut HeaderMap) -> Option<Encoding> {
    let content_type = headers.get(header::CONTENT_TYPE)?.to_str().ok()?;
    let rest = strip_prefix_ignore_case(content_type, GRPC_WEB)?;
    let (encoding, rest) = match strip_prefix_ignore_case(rest, TEXT) {
        Some(rest) => (Encoding::Text, rest),
        None => (Encoding::Binary, rest),
    };
    let grpc = HeaderValue::from_str(&format!("{GRPC}{rest}")).ok()?;
    headers.insert(header::CONTENT_TYPE, grpc);
    // gRPC servers answer with trailers only when asked to.
    headers.insert(header::TE, HeaderValue::from_static("trailers"));
    if encoding == Encoding::Text {
        headers.remove(header::CONTENT_LENGTH);
    }
    Some(encoding)
}

/// Request body for the backend: decoded from base64 for [`Encoding::Text`].
pub(crate) fn request_body(body: Body, encoding: Encoding) -> Body {
    match encoding {
        Encoding::Binary => body,
        Encoding::Text => Decoded {
            inner: body,
            pending: BytesMut::new(),
        }
        .boxed(),
    }
}

/// Turns a gRPC response into the gRPC-Web one the client asked for:
/// trailers become the last body frame. Responses that are not gRPC (an
/// error page of the backend) are left alone.
pub(crate) fn translate_response(response: &mut Response<Body>, encoding: Encoding) {
    let rest = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| strip_prefix_ignore_case(value, GRPC));
    let Some(rest) = rest else {
        return;
    };
    let text = match encoding {
        Encoding::Binary => "",
        Encoding::Text => TEXT,
    };
    let Ok(content_type) = HeaderValue::from_str(&format!("{GRPC_WEB}{text}{rest}")) else {
        return;
    };
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.remove(header::CONTENT_LENGTH);
    let inner = std::mem::replace(response.body_mut(), crate::http::body::empty());
    *response.body_mut() = Translated { inner, encoding }.boxed();
}

fn strip_prefix_ignore_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    let (head, rest) = value.split_at_checked(prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// gRPC-Web frame carrying `trailers` as an HTTP/1 header block.
fn trailers_frame(trailers: &HeaderMap) -> Bytes {
    let mut block = BytesMut::new();
    for (name, value) in trailers {
        block.put_slice(name.as_str().as_bytes());
        block.put_slice(b": ");
        block.put_slice(value.as_bytes());
        block.put_slice(b"\r\n");
    }
    let mut frame = BytesMut::with_capacity(5 + block.len());
    frame.put_u8(0x80);
    frame.put_u32(block.len() as u32);
    frame.put_slice(&block);
    frame.freeze()
}

/// gRPC response body as gRPC-Web.
struct Translated {
    inner: Body,
    encoding: Encoding,
}

impl hyper::body::Body for Translated {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let frame = match Pin::new(&mut self.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => frame,
            other => return other,
        };
        let data = match frame.into_data() {
            Ok(data) => data,
            Err(frame) => match frame.into_trailers() {
                Ok(trailers) => trailers_frame(&trailers),
                Err(frame) => return Poll::Ready(Some(Ok(frame))),
            },
        };
        // Every chunk is padded on its own, which the protocol allows, so
        // that no byte of a streamed message waits for the next one.
        let data = match self.encoding {
            Encoding::Binary => data,
            Encoding::Text => Bytes::from(STANDARD.encode(&data)),
        };
        Poll::Ready(Some(Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// Base64 request body, decoded. The source may be one base64 stream or
/// several padded ones in a row, cut anywhere.
struct Decoded {
    inner: Body,
    /// Characters received that do not make a group of four yet.
    pending: BytesMut,
}

impl Decoded {
    /// Decodes every whole group of four characters received so far.
    fn decode(&mut self, data: &[u8]) -> Result<Bytes, BodyError> {
        self.pending.extend_from_slice(data);
        let whole = self.pending.len() - self.pending.len() % 4;
        let mut groups: &[u8] = &self.pending[..whole];
        let mut decoded = Vec::with_capacity(whole / 4 * 3);
        while !groups.is_empty() {
            // Padding ends a base64 stream, the next group starts another.
            let end = match groups.iter().position(|byte| *byte == b'=') {
                Some(padding) => (padding / 4 + 1) * 4,
                None => groups.len(),
            };
            STANDARD
                .decode_vec(&groups[..end], &mut decoded)
                .map_err(|err| invalid(format!("grpc-web-text body: {err}")))?;
            groups = &groups[end..];
        }
        let _ = self.pending.split_to(whole);
        Ok(Bytes::from(decoded))
    }
}

fn invalid(message: String) -> BodyError {
    BodyError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

impl hyper::body::Body for Decoded {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            let frame = match Pin::new(&mut this.inner).poll_frame(context) {
                Poll::Ready(Some(Ok(frame))) => frame,
                Poll::Ready(None) if !this.pending.is_empty() => {
                    this.pending.clear();
                    let message = String::from("grpc-web-text body: truncated base64");
                    return Poll::Ready(Some(Err(invalid(message))));
                }
                other => return other,
            };
            match frame.into_data() {
                Ok(data) => match this.decode(&data) {
                    Ok(decoded) if decoded.is_empty() => {}
                    Ok(decoded) => return Poll::Ready(Some(Ok(Frame::data(decoded)))),
                    Err(err) => return Poll::Ready(Some(Err(err))),
                },
                Err(frame) => return Poll::Ready(Some(Ok(frame))),
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.pending.is_empty() && self.inner.is_end_stream()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn headers(content_type: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("8"));
        headers
    }

    fn decoder() -> Decoded {
        Decoded {
            inner: crate::http::body::empty(),
            pending: BytesMut::new(),
        }
    }

    #[test]
    fn request_headers_become_grpc() {
        let mut binary = headers("application/grpc-web+proto");
        assert_eq!(translate_request(&mut binary), Some(Encoding::Binary));
        assert_eq!(binary[header::CONTENT_TYPE], "application/grpc+proto");
        assert_eq!(binary[header::TE], "trailers");
        assert_eq!(binary[header::CONTENT_LENGTH], "8");

        let mut text = headers("Application/gRPC-Web-Text");
        assert_eq!(translate_request(&mut text), Some(Encoding::Text));
        assert_eq!(text[header::CONTENT_TYPE], "application/grpc");
        // The decoded body has another length.
        assert!(!text.contains_key(header::CONTENT_LENGTH));

        for other in ["application/grpc", "application/json", "application/grp"] {
            let mut untouched = headers(other);
            assert_eq!(translate_request(&mut untouched), None);
            assert_eq!(untouched[header::CONTENT_TYPE], other);
            assert!(!untouched.contains_key(header::TE));
        }
    }

    #[test]
    fn text_bodies_decode_across_chunks_and_padded_pieces() {
        let mut decoded = decoder();
        // "hello world" cut inside a group of four.
        assert_eq!(decoded.decode(b"aGVsbG8gd2").unwrap(), b"hello "[..]);
        assert_eq!(decoded.decode(b"9ybGQ=").unwrap(), b"world"[..]);
        assert!(decoded.pending.is_empty());

        // Two padded streams in one chunk, then a third.
        let mut decoded = decoder();
        assert_eq!(decoded.decode(b"YQ==Yg==Y2Q=").unwrap(), b"abcd"[..]);

        let mut decoded = decoder();
        assert_eq!(decoded.decode(b"YQ=").unwrap(), b""[..]);
        assert_eq!(decoded.pending.len(), 3);

        assert!(decoder().decode(b"!!!!").is_err());
        // Data after the padding of a group.
        assert!(decoder().decode(b"YQ=a").is_err());
    }

    #[test]
    fn trailers_become_a_flagged_frame() {
        let mut trailers = HeaderMap::new();
        trailers.insert("grpc-status", HeaderValue::from_static("0"));
        trailers.insert("grpc-message", HeaderValue::from_static("ok"));
        let frame = trailers_frame(&trailers);
        let block = b"grpc-status: 0\r\ngrpc-message: ok\r\n";
        assert_eq!(frame[..5], [0x80, 0, 0, 0, block.len() as u8]);
        assert_eq!(frame[5..], block[..]);
    }
}
