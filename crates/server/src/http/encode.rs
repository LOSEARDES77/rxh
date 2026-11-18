//! On the fly response compression (`encode`) and `Accept-Encoding` parsing.

use async_compression::tokio::bufread::{BrotliEncoder, GzipEncoder, ZstdEncoder};
use http_body_util::BodyExt;
use hyper::StatusCode;
use hyper::header::{self, HeaderMap, HeaderValue};
use rxh_core::config::Encoding;

use crate::http::body::{BodyReader, ReaderBody};
use crate::http::response::BoxResponse;

/// All `Accept-Encoding` values of a request joined into one string.
pub(crate) fn accept_encoding(headers: &HeaderMap) -> String {
    let values: Vec<&str> = headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    values.join(",")
}

/// Returns true if the `Accept-Encoding` value allows `encoding`. An explicit
/// entry beats `*`, and `q=0` forbids.
pub(crate) fn accepts(accept_encoding: &str, encoding: Encoding) -> bool {
    let mut explicit = None;
    let mut wildcard = None;
    for item in accept_encoding.split(',') {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let allowed = parts.all(|param| {
            let param = param.trim();
            let Some(quality) = param
                .strip_prefix("q=")
                .or_else(|| param.strip_prefix("Q="))
            else {
                return true;
            };
            let quality: f32 = quality.trim().parse().unwrap_or(1.0);
            quality > 0.0
        });
        if name.eq_ignore_ascii_case(encoding.token()) {
            explicit = Some(allowed);
        } else if name == "*" {
            wildcard = Some(allowed);
        }
    }
    explicit.or(wildcard).unwrap_or(false)
}

/// Returns true for content types worth compressing.
fn compressible(content_type: &str) -> bool {
    let content_type = content_type.to_ascii_lowercase();
    (content_type.starts_with("text/") && !content_type.starts_with("text/event-stream"))
        || ["json", "javascript", "xml", "svg", "wasm"]
            .iter()
            .any(|kind| content_type.contains(kind))
}

/// Adds `Vary: Accept-Encoding` unless a `Vary` already names it.
fn add_vary(headers: &mut HeaderMap) {
    let present = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').any(|name| {
                name.trim().eq_ignore_ascii_case("accept-encoding") || name.trim() == "*"
            })
        });
    if !present {
        headers.append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
}

/// Compresses `response` with the first of `offered` the client accepts. Leaves
/// it alone when it is already encoded, not compressible, partial or empty.
pub(crate) fn apply(
    response: BoxResponse,
    offered: &[Encoding],
    accept_encoding: &str,
) -> BoxResponse {
    let status = response.status();
    let headers = response.headers();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !compressible(content_type) {
        return response;
    }
    if status == StatusCode::NOT_MODIFIED {
        let (mut parts, body) = response.into_parts();
        add_vary(&mut parts.headers);
        return BoxResponse::from_parts(parts, body);
    }
    let no_transform = headers
        .get_all(header::CACHE_CONTROL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.to_ascii_lowercase().contains("no-transform"));
    if status.is_informational()
        || matches!(status, StatusCode::NO_CONTENT | StatusCode::PARTIAL_CONTENT)
        || headers.contains_key(header::CONTENT_ENCODING)
        || no_transform
    {
        return response;
    }

    let (mut parts, body) = response.into_parts();
    add_vary(&mut parts.headers);
    let Some(encoding) = offered
        .iter()
        .copied()
        .find(|encoding| accepts(accept_encoding, *encoding))
    else {
        return BoxResponse::from_parts(parts, body);
    };

    let reader = BodyReader::new(body);
    let slot = reader.error_slot();
    let body = match encoding {
        Encoding::Gzip => ReaderBody::new(GzipEncoder::new(reader))
            .with_source_error(slot)
            .boxed(),
        Encoding::Zstd => ReaderBody::new(ZstdEncoder::new(reader))
            .with_source_error(slot)
            .boxed(),
        Encoding::Br => ReaderBody::new(BrotliEncoder::new(reader))
            .with_source_error(slot)
            .boxed(),
    };
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(encoding.token()),
    );
    if let Some(etag) = parts.headers.get(header::ETAG)
        && !etag.as_bytes().starts_with(b"W/")
        && let Ok(weak) = HeaderValue::from_bytes(&[b"W/", etag.as_bytes()].concat())
    {
        parts.headers.insert(header::ETAG, weak);
    }
    BoxResponse::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_encoding_parsing() {
        assert!(accepts("gzip, br", Encoding::Br));
        assert!(!accepts("gzip", Encoding::Br));
        assert!(accepts("*", Encoding::Zstd));
        assert!(!accepts("gzip;q=0, *", Encoding::Gzip));
        assert!(accepts("GZIP;q=0.5", Encoding::Gzip));
        assert!(!accepts("", Encoding::Gzip));
    }

    #[test]
    fn compressible_types() {
        assert!(compressible("text/html; charset=utf-8"));
        assert!(compressible("application/json"));
        assert!(compressible("image/svg+xml"));
        assert!(!compressible("image/png"));
        assert!(!compressible("text/event-stream"));
    }
}
