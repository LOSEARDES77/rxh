//! Allocator calls of a proxied request, see `docs/profiling.md`. The client
//! and the backend of this test do not allocate once they are connected, so
//! every call counted while requests flow is made by the proxy.

mod util;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

/// Most allocator calls a small `GET` may make. It made 84 before the header
/// values, the access line and the idle pool keys were built without
/// allocating, and makes 26 now; the margin is for changes in hyper.
const MAX_CALLS_PER_REQUEST: u64 = 40;

const REQUEST: &[u8] = b"GET /path?q=1 HTTP/1.1\r\nHost: example.com\r\n\r\n";
const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";

static CALLS: AtomicU64 = AtomicU64::new(0);

/// System allocator that counts `alloc`, `alloc_zeroed` and `realloc` calls.
struct Counting;

// SAFETY: every method forwards its arguments unchanged to `System`, which
// upholds the contract of `GlobalAlloc`; counting has no effect on memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds the contract of `GlobalAlloc::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds the contract of `GlobalAlloc::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds the contract of `GlobalAlloc::realloc`.
        unsafe { System.realloc(pointer, layout, new_size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds the contract of `GlobalAlloc::dealloc`.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Reads from `stream` until the bytes read end with `end`. False when the
/// peer closed the connection first.
fn read_until(stream: &mut TcpStream, end: &[u8]) -> bool {
    let mut buffer = [0_u8; 2048];
    let mut len = 0;
    while !buffer[..len].ends_with(end) {
        match stream.read(&mut buffer[len..]) {
            Ok(read) if read > 0 => len += read,
            _ => return false,
        }
    }
    true
}

/// Keep-alive backend on threads, answering every request with [`RESPONSE`].
fn backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept");
            thread::spawn(move || {
                while read_until(&mut stream, b"\r\n\r\n") {
                    stream.write_all(RESPONSE).expect("write response");
                }
            });
        }
    });
    address
}

fn get(client: &mut TcpStream) {
    client.write_all(REQUEST).expect("write request");
    assert!(read_until(client, b"\r\n\r\nok"), "the proxy answers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proxied_request_makes_few_allocator_calls() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let log = directory.path().join("access.log");
    let backend = backend();
    let proxy = util::spawn(&format!(
        "[log]\noutput = '{}'\n\n[[server]]\nlisten = \"127.0.0.1:0\"\nforward = \"{backend}\"\n",
        log.display()
    ))[0];

    let (warm_up, measured) = (100, 1000);
    let calls = tokio::task::block_in_place(|| {
        let mut client = TcpStream::connect(proxy).expect("connect");
        // Connections, buffers and tables reach their final size.
        for _ in 0..warm_up {
            get(&mut client);
        }
        let before = CALLS.load(Ordering::Relaxed);
        for _ in 0..measured {
            get(&mut client);
        }
        CALLS.load(Ordering::Relaxed) - before
    });

    let per_request = calls / measured;
    assert!(
        per_request <= MAX_CALLS_PER_REQUEST,
        "{per_request} allocator calls per request: the hot path allocates again, \
         see docs/profiling.md"
    );

    // The reused line buffer still gives one complete line per request.
    let lines = std::fs::read_to_string(&log).expect("access log");
    assert_eq!(lines.lines().count() as u64, warm_up + measured);
    for line in lines.lines() {
        assert!(line.contains(" GET /path?q=1 HTTP 200 OK "), "{line}");
    }
}
