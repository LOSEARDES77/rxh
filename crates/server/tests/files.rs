//! File server: streaming, ranges, conditional requests, hiding, precompressed
//! files and on the fly compression.

mod util;

use std::net::SocketAddr;

use async_compression::tokio::bufread::GzipDecoder;
use bytes::Bytes;
use http_body_util::Empty;
use hyper::{HeaderMap, Method, Request, StatusCode};
use tokio::io::AsyncReadExt;

async fn request(
    address: SocketAddr,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    util::send(address, builder.body(Empty::new()).expect("valid request")).await
}

async fn get(
    address: SocketAddr,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    request(address, Method::GET, uri, headers).await
}

fn serve(dir: &tempfile::TempDir) -> SocketAddr {
    let text = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = '{}'\n",
        dir.path().display()
    );
    util::spawn(&text)[0]
}

fn serve_table(table: &str, dir: &tempfile::TempDir) -> SocketAddr {
    let text = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = {{ root = '{}', {table} }}\n",
        dir.path().display()
    );
    util::spawn(&text)[0]
}

async fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzipDecoder::new(data)
        .read_to_end(&mut out)
        .await
        .expect("valid gzip");
    out
}

#[tokio::test]
async fn ranges() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "0123456789").expect("write");
    let address = serve(&dir);

    let (status, headers, body) = get(address, "/f.txt", &[("range", "bytes=2-4")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 2-4/10");
    assert_eq!(headers["content-length"], "3");
    assert_eq!(&body[..], b"234");

    let (status, headers, body) = get(address, "/f.txt", &[("range", "bytes=7-")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-range"], "bytes 7-9/10");
    assert_eq!(&body[..], b"789");

    let (status, _, body) = get(address, "/f.txt", &[("range", "bytes=-2")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], b"89");

    let (status, headers, _) = get(address, "/f.txt", &[("range", "bytes=10-20")]).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers["content-range"], "bytes */10");

    let (status, _, body) = get(address, "/f.txt", &[("range", "bytes=0-1,4-5")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"0123456789");

    let (_, headers, _) = get(address, "/f.txt", &[]).await;
    assert_eq!(headers["accept-ranges"], "bytes");
    assert!(headers.contains_key("last-modified"));
}

#[tokio::test]
async fn conditional_requests() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "hello").expect("write");
    let address = serve(&dir);

    let (_, headers, _) = get(address, "/f.txt", &[]).await;
    let etag = headers["etag"].to_str().expect("etag").to_owned();
    let modified = headers["last-modified"].to_str().expect("date").to_owned();
    assert!(etag.starts_with("W/\""));

    let (status, headers, body) = get(address, "/f.txt", &[("if-none-match", &etag)]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["etag"], etag.as_str());
    assert!(body.is_empty());

    let (status, _, body) = get(address, "/f.txt", &[("if-none-match", "W/\"other\"")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"hello");

    let (status, _, _) = get(address, "/f.txt", &[("if-modified-since", &modified)]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn head_has_no_body() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "hello").expect("write");
    let address = serve(&dir);

    let (status, headers, body) = request(address, Method::HEAD, "/f.txt", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], "5");
    assert!(body.is_empty());
}

#[tokio::test]
async fn browse_false_and_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("empty")).expect("mkdir");
    std::fs::create_dir(dir.path().join("site")).expect("mkdir");
    std::fs::write(dir.path().join("site/home.htm"), "home").expect("write");
    let address = serve_table("browse = false, index = [\"home.htm\"]", &dir);

    let (status, _, _) = get(address, "/empty/", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, body) = get(address, "/site/", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"home");
}

#[tokio::test]
async fn hide_patterns() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join(".git")).expect("mkdir");
    std::fs::write(dir.path().join(".git/config"), "x").expect("write");
    std::fs::write(dir.path().join("key.pem"), "x").expect("write");
    std::fs::write(dir.path().join("ok.txt"), "ok").expect("write");
    let address = serve_table("hide = [\".git\", \"*.pem\"]", &dir);

    for uri in ["/.git/config", "/key.pem", "/%2egit/config"] {
        assert_eq!(
            get(address, uri, &[]).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert_eq!(get(address, "/ok.txt", &[]).await.0, StatusCode::OK);
    let (_, _, listing) = get(address, "/", &[]).await;
    let listing = String::from_utf8_lossy(&listing);
    assert!(listing.contains("ok.txt"));
    assert!(!listing.contains(".git") && !listing.contains("key.pem"));
}

#[tokio::test]
async fn precompressed_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("app.js"), "plain").expect("write");
    std::fs::write(dir.path().join("app.js.gz"), "pretend gzip").expect("write");
    let address = serve_table("precompressed = [\"gzip\"]", &dir);

    let (status, headers, body) = get(address, "/app.js", &[("accept-encoding", "gzip")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-encoding"], "gzip");
    assert_eq!(headers["vary"], "Accept-Encoding");
    assert!(
        headers["content-type"]
            .to_str()
            .expect("type")
            .starts_with("text/javascript")
    );
    assert_eq!(&body[..], b"pretend gzip");

    let (_, headers, body) = get(address, "/app.js", &[("accept-encoding", "br")]).await;
    assert!(!headers.contains_key("content-encoding"));
    assert_eq!(&body[..], b"plain");
}

#[tokio::test]
async fn encode_compresses_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = "compress me ".repeat(1000);
    std::fs::write(dir.path().join("a.txt"), &text).expect("write");
    std::fs::write(dir.path().join("b.png"), &text).expect("write");
    let config = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = '{}'\nencode = [\"zstd\", \"gzip\"]\n",
        dir.path().display()
    );
    let address = util::spawn(&config)[0];

    let (_, headers, body) = get(address, "/a.txt", &[("accept-encoding", "gzip")]).await;
    assert_eq!(headers["content-encoding"], "gzip");
    assert_eq!(headers["vary"], "Accept-Encoding");
    assert!(!headers.contains_key("content-length"));
    assert_eq!(gunzip(&body).await, text.as_bytes());

    let (_, headers, body) = get(address, "/a.txt", &[("accept-encoding", "identity")]).await;
    assert!(!headers.contains_key("content-encoding"));
    assert_eq!(&body[..], text.as_bytes());

    let (_, headers, _) = get(address, "/b.png", &[("accept-encoding", "gzip")]).await;
    assert!(!headers.contains_key("content-encoding"));

    let (status, headers, _) = get(
        address,
        "/a.txt",
        &[("accept-encoding", "gzip"), ("range", "bytes=0-9")],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert!(!headers.contains_key("content-encoding"));
}

#[tokio::test]
async fn large_file_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut state: u32 = 12345;
    let data: Vec<u8> = (0..8 * 1024 * 1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect();
    std::fs::write(dir.path().join("big.bin"), &data).expect("write");
    let address = serve(&dir);

    let (status, headers, body) = get(address, "/big.bin", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-length"], data.len().to_string().as_str());
    assert!(body[..] == data[..], "body differs from the file");

    let (status, _, body) = get(address, "/big.bin", &[("range", "bytes=1000000-1000099")]).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], &data[1_000_000..1_000_100]);
}

#[tokio::test]
async fn encode_compresses_proxied_responses() {
    use http_body_util::Full;
    use hyper::Response;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let backend = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let service = service_fn(|_| async {
                Ok::<_, std::convert::Infallible>(
                    Response::builder()
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from_static(b"{\"proxied\":true}")))
                        .expect("response"),
                )
            });
            tokio::task::spawn(
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service),
            );
        }
    });
    let config = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\nencode = [\"gzip\"]\n"
    );
    let address = util::spawn(&config)[0];

    let (_, headers, body) = get(address, "/", &[("accept-encoding", "gzip")]).await;
    assert_eq!(headers["content-encoding"], "gzip");
    assert_eq!(gunzip(&body).await, b"{\"proxied\":true}");
}

/// Root `outer/root` with `outer/secret.txt` next to it.
fn jail() -> (tempfile::TempDir, std::path::PathBuf) {
    let outer = tempfile::tempdir().expect("tempdir");
    let root = outer.path().join("root");
    std::fs::create_dir(&root).expect("mkdir");
    std::fs::write(outer.path().join("secret.txt"), "secret").expect("write");
    std::fs::write(root.join("ok.txt"), "ok").expect("write");
    (outer, root)
}

fn serve_path(root: &std::path::Path, extra: &str) -> SocketAddr {
    let text = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = {{ root = '{}'{extra} }}\n",
        root.display()
    );
    util::spawn(&text)[0]
}

#[tokio::test]
async fn traversal_is_refused() {
    let (_outer, root) = jail();
    std::fs::create_dir(root.join(".git")).expect("mkdir");
    std::fs::write(root.join(".git/config"), "x").expect("write");
    let address = serve_path(&root, ", hide = [\".git\"]");

    for uri in [
        "/%2e%2e/secret.txt",
        "/..%2fsecret.txt",
        "/%2E%2E%2Fsecret.txt",
        "/.git%2fconfig",
        "/%2egit%2Fconfig",
    ] {
        assert_eq!(
            get(address, uri, &[]).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert_eq!(
        get(address, "/ok.txt%00", &[]).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(get(address, "/%00", &[]).await.0, StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_out_of_root_are_refused() {
    let (outer, root) = jail();
    std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("link.txt"))
        .expect("symlink");
    std::fs::create_dir(root.join("site")).expect("mkdir");
    std::os::unix::fs::symlink(
        outer.path().join("secret.txt"),
        root.join("site/index.html"),
    )
    .expect("symlink");
    std::os::unix::fs::symlink(outer.path(), root.join("dir")).expect("symlink");
    let address = serve_path(&root, ", browse = false");

    for uri in ["/link.txt", "/site/", "/site/index.html", "/dir/secret.txt"] {
        assert_eq!(
            get(address, uri, &[]).await.0,
            StatusCode::NOT_FOUND,
            "{uri}"
        );
    }
    assert_eq!(get(address, "/ok.txt", &[]).await.0, StatusCode::OK);
}

#[tokio::test]
async fn method_not_allowed() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "x").expect("write");
    let address = serve(&dir);

    let (status, headers, _) = request(address, Method::POST, "/f.txt", &[]).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers["allow"], "GET, HEAD");
}

#[tokio::test]
async fn if_range_and_empty_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("f.txt"), "0123456789").expect("write");
    std::fs::write(dir.path().join("empty.txt"), "").expect("write");
    let address = serve(&dir);

    let (_, headers, _) = get(address, "/f.txt", &[]).await;
    let modified = headers["last-modified"].to_str().expect("date").to_owned();
    let (status, _, body) = get(
        address,
        "/f.txt",
        &[("range", "bytes=0-1"), ("if-range", &modified)],
    )
    .await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(&body[..], b"01");
    let stale = "Thu, 01 Jan 1970 00:00:00 GMT";
    let (status, _, body) = get(
        address,
        "/f.txt",
        &[("range", "bytes=0-1"), ("if-range", stale)],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"0123456789");

    for range in ["bytes=0-", "bytes=0-0", "bytes=-1"] {
        let (status, headers, _) = get(address, "/empty.txt", &[("range", range)]).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE, "{range}");
        assert_eq!(headers["content-range"], "bytes */0");
    }
    assert_eq!(get(address, "/empty.txt", &[]).await.0, StatusCode::OK);
}

#[tokio::test]
async fn listing_links_are_percent_encoded() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a#b%41 c?.txt"), "odd").expect("write");
    let address = serve(&dir);

    let (_, _, listing) = get(address, "/", &[]).await;
    let listing = String::from_utf8_lossy(&listing);
    assert!(
        listing.contains("href=\"/a%23b%2541%20c%3F.txt\""),
        "{listing}"
    );
    let (status, _, body) = get(address, "/a%23b%2541%20c%3F.txt", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"odd");
}

#[tokio::test]
async fn not_modified_keeps_vary_for_encoded_routes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "text").expect("write");
    let config = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nserve = '{}'\nencode = [\"gzip\"]\n",
        dir.path().display()
    );
    let address = util::spawn(&config)[0];
    let (_, headers, _) = get(address, "/a.txt", &[]).await;
    let etag = headers["etag"].to_str().expect("etag").to_owned();
    let (status, headers, _) = get(
        address,
        "/a.txt",
        &[("if-none-match", &etag), ("accept-encoding", "gzip")],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers["vary"], "Accept-Encoding");
}

#[tokio::test]
async fn encode_does_not_hide_upstream_truncation() {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let backend = listener.local_addr().expect("address");
    tokio::task::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::task::spawn(async move {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await;
                let head =
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 1000\r\n\r\n";
                let _ = stream
                    .write_all(format!("{head}only ten b").as_bytes())
                    .await;
            });
        }
    });
    let config = format!(
        "[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\nencode = [\"gzip\"]\n"
    );
    let address = util::spawn(&config)[0];

    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    stream
        .write_all(
            b"GET / HTTP/1.1\r\nhost: x\r\naccept-encoding: gzip\r\nconnection: close\r\n\r\n",
        )
        .await
        .expect("send");
    let mut received = Vec::new();
    let _ = stream.read_to_end(&mut received).await;
    assert!(
        !received.ends_with(b"0\r\n\r\n"),
        "a truncated upstream body must not end as a complete chunked body"
    );
}
