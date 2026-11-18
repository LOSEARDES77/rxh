//! Static file server: streaming, ranges, conditional requests, precompressed
//! files and a directory listing page.

use std::ffi::OsString;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::{Method, StatusCode};
use rxh_core::config::{Encoding, FileServer};
use rxh_core::matcher::PathPattern;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::http::body::{self, ReaderBody};
use crate::http::encode;
use crate::http::response::{self, BoxResponse};

const FILE_EXPLORER_TEMPLATE: &str = include_str!("../assets/file_explorer.html");
const NOT_FOUND_TEMPLATE: &str = include_str!("../assets/404.html");

/// Compiled settings of a `serve` action.
#[derive(Debug)]
pub(crate) struct Files {
    root: String,
    browse: bool,
    index: Vec<String>,
    hide: Vec<PathPattern>,
    precompressed: Vec<Encoding>,
}

/// What a `Range` header asks for.
#[derive(Debug, PartialEq, Eq)]
enum ByteRange {
    /// Serve the whole file.
    Full,
    /// Serve bytes `start..=end`.
    Partial(u64, u64),
    /// Nothing of the file is in range.
    Unsatisfiable,
}

impl Files {
    pub(crate) fn new(config: &FileServer) -> Self {
        Self {
            root: config.root.clone(),
            browse: config.browse,
            index: config.index.clone(),
            hide: config
                .hide
                .iter()
                .filter_map(|pattern| PathPattern::new(&format!("/{pattern}")).ok())
                .collect(),
            precompressed: config.precompressed.clone(),
        }
    }

    /// Returns true if a path component matches a `hide` entry.
    fn hidden(&self, name: &str) -> bool {
        let name = format!("/{name}");
        self.hide.iter().any(|pattern| pattern.matches(&name))
    }

    fn hidden_path(&self, path: &Path) -> bool {
        path.components()
            .any(|part| self.hidden(&part.as_os_str().to_string_lossy()))
    }

    /// Answers a `GET` or `HEAD` for `path` (the request path, starting with
    /// "/") with a file or a directory listing found under the root, or a 404.
    pub(crate) async fn transfer(
        &self,
        method: &Method,
        headers: &HeaderMap,
        path: &str,
        mount_uri: &str,
    ) -> BoxResponse {
        let mount_uri = normalize_mount_uri(mount_uri);
        if method != Method::GET && method != Method::HEAD {
            let mut response = response::plain(StatusCode::METHOD_NOT_ALLOWED, "HTTP 405");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return response;
        }
        let Some(relative_path) = strip_mount_prefix(path, &mount_uri) else {
            return not_found_html(&mount_uri);
        };
        let relative_path = percent_decode(relative_path);
        if relative_path.contains('\0') {
            return response::plain(StatusCode::BAD_REQUEST, "HTTP 400 BAD REQUEST");
        }
        if self.hidden_path(Path::new(&relative_path)) {
            return not_found_html(&mount_uri);
        }

        let Ok(directory) = Path::new(&self.root).canonicalize() else {
            return not_found_html(&mount_uri);
        };
        let Ok(file) = directory.join(&relative_path).canonicalize() else {
            return not_found_html(&mount_uri);
        };
        let hidden = file
            .strip_prefix(&directory)
            .map_or(true, |inside| self.hidden_path(inside));
        if hidden {
            return not_found_html(&mount_uri);
        }

        if file.is_dir() {
            for name in &self.index {
                if self.hidden(name) {
                    continue;
                }
                // Same checks as the requested path: symlinks must not leave the root.
                let Ok(index) = file.join(name).canonicalize() else {
                    continue;
                };
                let inside = index
                    .strip_prefix(&directory)
                    .is_ok_and(|inside| !self.hidden_path(inside));
                if inside && index.is_file() {
                    return self.serve_file(&index, &directory, method, headers).await;
                }
            }
            if !self.browse {
                return not_found_html(&mount_uri);
            }
            return self.explorer_html(&directory, &file, &mount_uri).await;
        }

        if !file.is_file() {
            return not_found_html(&mount_uri);
        }
        self.serve_file(&file, &directory, method, headers).await
    }

    async fn serve_file(
        &self,
        file: &Path,
        root: &Path,
        method: &Method,
        headers: &HeaderMap,
    ) -> BoxResponse {
        let accepted = encode::accept_encoding(headers);
        let mut served = (file.to_path_buf(), None);
        for encoding in &self.precompressed {
            if !encode::accepts(&accepted, *encoding) {
                continue;
            }
            let mut name: OsString = file.as_os_str().to_owned();
            name.push(match encoding {
                Encoding::Zstd => ".zst",
                Encoding::Br => ".br",
                Encoding::Gzip => ".gz",
            });
            let Ok(sidecar) = PathBuf::from(name).canonicalize() else {
                continue;
            };
            if sidecar.starts_with(root) && sidecar.is_file() {
                served = (sidecar, Some(*encoding));
                break;
            }
        }
        let (path, encoding) = served;

        let Ok(mut opened) = File::open(&path).await else {
            return not_found_html("/");
        };
        let Ok(metadata) = opened.metadata().await else {
            return not_found_html("/");
        };
        let length = metadata.len();
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        let nanos = modified
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let etag = format!("W/\"{length:x}-{nanos:x}\"");
        let last_modified = httpdate::fmt_http_date(modified);

        let mut builder = response::builder()
            .header(header::CONTENT_TYPE, content_type(file))
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::LAST_MODIFIED, &last_modified)
            .header(header::ETAG, &etag);
        if !self.precompressed.is_empty() {
            builder = builder.header(header::VARY, "Accept-Encoding");
        }
        if let Some(encoding) = encoding {
            builder = builder.header(header::CONTENT_ENCODING, encoding.token());
        }

        if not_modified(headers, &etag, modified) {
            return builder
                .status(StatusCode::NOT_MODIFIED)
                .body(body::empty())
                .expect("static headers are valid");
        }

        // A validator that does not match the file means the range is stale.
        let if_range_ok = headers
            .get(header::IF_RANGE)
            .is_none_or(|value| value.to_str().is_ok_and(|value| value == last_modified));
        let range = headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok())
            .filter(|_| if_range_ok)
            .map_or(ByteRange::Full, |value| parse_range(value, length));

        let (status, start, count) = match range {
            ByteRange::Full => (StatusCode::OK, 0, length),
            ByteRange::Partial(start, end) => {
                builder = builder.header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{length}"),
                );
                (StatusCode::PARTIAL_CONTENT, start, end - start + 1)
            }
            ByteRange::Unsatisfiable => {
                return builder
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                    .header(header::CONTENT_LENGTH, 0)
                    .body(body::empty())
                    .expect("static headers are valid");
            }
        };
        builder = builder.status(status).header(header::CONTENT_LENGTH, count);

        let body = if method == Method::HEAD {
            body::empty()
        } else {
            if start > 0 && opened.seek(SeekFrom::Start(start)).await.is_err() {
                return not_found_html("/");
            }
            http_body_util::BodyExt::boxed(
                ReaderBody::new(opened.take(count)).labeled(path.display().to_string()),
            )
        };
        builder.body(body).expect("static headers are valid")
    }

    async fn explorer_html(&self, root: &Path, dir: &Path, mount_uri: &str) -> BoxResponse {
        let relative = dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let mut entries = Vec::new();

        if dir != root {
            let mut parent_display = mount_uri.to_string();
            if !relative.is_empty() {
                let parent = relative
                    .split('/')
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>();
                if parent.len() > 1 {
                    parent_display = join_url(
                        mount_uri,
                        &format!("{}/", parent[..parent.len() - 1].join("/")),
                    );
                }
            }

            entries.push(format!(
            r#"<li class="file-item"><a href="{}"><svg class="file-icon" viewBox="0 0 20 20"><path d="M2 6a2 2 0 012-2h5l2 2h5a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z"/></svg>..</a></li>"#,
            escape_html(&encode_path(&parent_display))
        ));
        }

        let Ok(mut dir_entries) = tokio::fs::read_dir(dir).await else {
            return not_found_html(mount_uri);
        };

        while let Ok(Some(entry)) = dir_entries.next_entry().await {
            let Ok(metadata) = entry.metadata().await else {
                continue;
            };

            let raw_name = entry.file_name().to_string_lossy().to_string();
            if self.hidden(&raw_name) {
                continue;
            }
            let name = escape_html(&raw_name);
            let is_dir = metadata.is_dir();
            let size = if is_dir {
                String::new()
            } else {
                format!("{:.1} KB", metadata.len() as f64 / 1024.0)
            };

            let icon = if is_dir {
                r#"<path d="M2 6a2 2 0 012-2h5l2 2h5a2 2 0 012 2v6a2 2 0 01-2 2H4a2 2 0 01-2-2V6z"/>"#
            } else {
                r#"<path d="M4 4a2 2 0 012-2h4.586A2 2 0 0112 2.586L15.414 6A2 2 0 0116 7.414V16a2 2 0 01-2 2H6a2 2 0 01-2-2V4z"/>"#
            };

            let href = if relative.is_empty() {
                join_url(
                    mount_uri,
                    &format!("{}{}", raw_name, if is_dir { "/" } else { "" }),
                )
            } else {
                join_url(
                    mount_uri,
                    &format!(
                        "{}/{}{}",
                        relative.trim_end_matches('/'),
                        raw_name,
                        if is_dir { "/" } else { "" }
                    ),
                )
            };

            entries.push(format!(
            r#"<li class="file-item"><a href="{}"><svg class="file-icon" viewBox="0 0 20 20">{}</svg>{}</a><span class="file-info"><span class="file-size">{}</span></span></li>"#,
            escape_html(&encode_path(&href)), icon, name, size
        ));
        }

        let cwd = if relative.is_empty() {
            mount_uri.to_string()
        } else {
            join_url(mount_uri, relative.trim_end_matches('/'))
        };

        let html = FILE_EXPLORER_TEMPLATE
            .replace("{cwd}", &escape_html(&cwd))
            .replace("{home}", mount_uri)
            .replace("{list}", &entries.join("\n"));

        response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(body::full(html))
            .expect("static headers are valid")
    }
}

/// Evaluates `If-None-Match`, or else `If-Modified-Since`.
fn not_modified(headers: &HeaderMap, etag: &str, modified: SystemTime) -> bool {
    if let Some(value) = headers.get(header::IF_NONE_MATCH) {
        let tag = |tag: &str| tag.trim().trim_start_matches("W/").to_owned();
        return value.to_str().is_ok_and(|list| {
            list.split(',')
                .any(|item| item.trim() == "*" || tag(item) == tag(etag))
        });
    }
    let Some(since) = headers
        .get(header::IF_MODIFIED_SINCE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| httpdate::parse_http_date(value).ok())
    else {
        return false;
    };
    let seconds = |time: SystemTime| time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    seconds(modified) <= seconds(since)
}

/// Parses a single `bytes=` range. Malformed or multiple ranges mean the
/// whole file.
fn parse_range(value: &str, length: u64) -> ByteRange {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return ByteRange::Full;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return ByteRange::Full;
    };
    let (first, last) = (first.trim(), last.trim());
    let start: Option<u64> = first.parse().ok();
    let end: Option<u64> = last.parse().ok();
    match (start, end) {
        (Some(start), None) if last.is_empty() => {
            if start >= length {
                ByteRange::Unsatisfiable
            } else {
                ByteRange::Partial(start, length - 1)
            }
        }
        (Some(start), Some(end)) if start <= end => {
            if start >= length {
                ByteRange::Unsatisfiable
            } else {
                ByteRange::Partial(start, end.min(length - 1))
            }
        }
        (None, Some(suffix)) if first.is_empty() => {
            if suffix == 0 || length == 0 {
                ByteRange::Unsatisfiable
            } else {
                ByteRange::Partial(length.saturating_sub(suffix), length - 1)
            }
        }
        _ => ByteRange::Full,
    }
}

/// Percent-encodes everything in a URL path except unreserved characters and `/`.
fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Decodes `%XX` escapes. Invalid escapes stay as they are.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| bytes.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                at += 3;
            }
            None => {
                decoded.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn content_type(path: &Path) -> &'static str {
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
    match extension.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "xml" => "application/xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        _ => "application/octet-stream",
    }
}

fn not_found_html(home: &str) -> BoxResponse {
    let html = NOT_FOUND_TEMPLATE.replace("{home}", home);

    response::builder()
        .status(http::StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(body::full(html))
        .expect("static headers are valid")
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn normalize_mount_uri(uri: &str) -> String {
    let mut uri = if uri.trim().is_empty() {
        String::from("/")
    } else if uri.starts_with('/') {
        uri.to_string()
    } else {
        format!("/{uri}")
    };

    if uri != "/" {
        uri = uri.trim_end_matches('/').to_string();
    }

    uri
}

fn strip_mount_prefix<'a>(path: &'a str, mount_uri: &str) -> Option<&'a str> {
    let path = if path.starts_with('/') {
        path
    } else {
        return None;
    };

    if mount_uri == "/" {
        return Some(path.trim_start_matches('/'));
    }

    if path == mount_uri {
        return Some("");
    }

    let with_slash = format!("{mount_uri}/");
    path.strip_prefix(&with_slash)
}

fn join_url(base: &str, suffix: &str) -> String {
    if base == "/" {
        if suffix.is_empty() {
            String::from("/")
        } else {
            format!("/{}", suffix.trim_start_matches('/'))
        }
    } else if suffix.is_empty() {
        base.to_string()
    } else {
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }
}
