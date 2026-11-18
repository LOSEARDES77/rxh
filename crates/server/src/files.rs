//! Static file server with a directory listing page.

use std::path::Path;

use hyper::header;

use crate::http::body;
use crate::http::response::{self, BoxResponse};

const FILE_EXPLORER_TEMPLATE: &str =
    include_str!(concat!(env!("OUT_DIR"), "/file_explorer.min.html"));
const NOT_FOUND_TEMPLATE: &str = include_str!(concat!(env!("OUT_DIR"), "/404.min.html"));

/// Returns an HTTP response whose body is the content of a file. The file
/// must be located inside the root directory specified by the configuration
/// and must be readable, otherwise a 404 response is returned. This function
/// also assumes that `path` is relative, so it can't start with "/".
pub(crate) async fn transfer(path: &str, root: &str, mount_uri: &str) -> BoxResponse {
    let mount_uri = normalize_mount_uri(mount_uri);
    let Some(relative_path) = strip_mount_prefix(path, &mount_uri) else {
        return not_found_html(&mount_uri);
    };

    let Ok(directory) = Path::new(root).canonicalize() else {
        return not_found_html(&mount_uri);
    };

    let target = if relative_path.is_empty() {
        directory.clone()
    } else {
        directory.join(relative_path)
    };

    let Ok(file) = target.canonicalize() else {
        return not_found_html(&mount_uri);
    };

    if !file.starts_with(&directory) {
        return not_found_html(&mount_uri);
    }

    if file.is_dir() {
        let index = file.join("index.html");
        if index.exists() && index.is_file() {
            return serve_file(&index).await;
        }

        return explorer_html(&directory, &file, &mount_uri).await;
    }

    if !file.is_file() {
        return not_found_html(&mount_uri);
    }

    serve_file(&file).await
}

async fn serve_file(file: &Path) -> BoxResponse {
    let content_type = content_type(file);

    // TODO: gzip medium files, stream large files and set
    // Transfer-Encoding: chunked.
    match tokio::fs::read(file).await {
        Ok(content) => response::builder()
            .header(header::CONTENT_TYPE, content_type)
            .body(body::full(content))
            .expect("static headers are valid"),

        Err(_) => not_found_html("/"),
    }
}

async fn explorer_html(root: &Path, dir: &Path, mount_uri: &str) -> BoxResponse {
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
            parent_display
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
            escape_html(&href), icon, name, size
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
