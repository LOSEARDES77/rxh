//! Zero copy relay between two TCP sockets on Linux: `splice(2)` moves the
//! bytes from a socket to a pipe and from the pipe to the other socket inside
//! the kernel, so they never visit user space.

use std::future::Future;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr;
use std::time::Duration;

use tokio::io::Interest;
use tokio::net::TcpStream;

/// Most bytes asked of one `splice` call: the default capacity of a pipe.
const CHUNK: usize = 64 * 1024;

/// Copies `from` to `to` until `from` reaches end of file, then shuts down the
/// write side of `to`. Returns the bytes copied; `moved` also receives them as
/// they reach `to`.
///
/// Fails with `TimedOut` when `from` stays unreadable for `read_limit` or `to`
/// stays unwritable for `write_limit`.
pub(crate) async fn copy(
    from: &TcpStream,
    to: &TcpStream,
    read_limit: Option<Duration>,
    write_limit: Option<Duration>,
    moved: impl Fn(u64),
) -> io::Result<u64> {
    let (pipe_out, pipe_in) = io::pipe()?;
    let mut total: u64 = 0;
    loop {
        // The pipe is empty here, so `WouldBlock` can only mean that the
        // socket has nothing to read.
        let filled = loop {
            wait(from.readable(), read_limit).await?;
            let spliced = from.try_io(Interest::READABLE, || {
                splice(from.as_fd(), pipe_in.as_fd(), CHUNK)
            });
            match spliced {
                Ok(filled) => break filled,
                Err(err) if retry(&err) => {}
                Err(err) => return Err(err),
            }
        };
        if filled == 0 {
            shutdown_write(to)?;
            return Ok(total);
        }

        let mut left = filled;
        while left > 0 {
            wait(to.writable(), write_limit).await?;
            match to.try_io(Interest::WRITABLE, || {
                splice(pipe_out.as_fd(), to.as_fd(), left)
            }) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(drained) => {
                    left -= drained;
                    total += drained as u64;
                    moved(drained as u64);
                }
                Err(err) if retry(&err) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

fn retry(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

async fn wait(
    ready: impl Future<Output = io::Result<()>>,
    limit: Option<Duration>,
) -> io::Result<()> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, ready)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no progress"))?,
        None => ready.await,
    }
}

/// Moves up to `len` bytes without blocking; one of the two ends is a pipe.
fn splice(from: BorrowedFd<'_>, to: BorrowedFd<'_>, len: usize) -> io::Result<usize> {
    // SAFETY: the borrows keep both descriptors open during the call, and
    // null offsets are what sockets and pipes require.
    let moved = unsafe {
        libc::splice(
            from.as_raw_fd(),
            ptr::null_mut(),
            to.as_raw_fd(),
            ptr::null_mut(),
            len,
            libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
        )
    };
    usize::try_from(moved).map_err(|_| io::Error::last_os_error())
}

fn shutdown_write(stream: &TcpStream) -> io::Result<()> {
    // SAFETY: the borrow keeps the socket open during the call.
    match unsafe { libc::shutdown(stream.as_raw_fd(), libc::SHUT_WR) } {
        0 => Ok(()),
        _ => Err(io::Error::last_os_error()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Two connected sockets.
    async fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (near, far) = tokio::join!(TcpStream::connect(address), listener.accept());
        (near.unwrap(), far.unwrap().0)
    }

    #[tokio::test]
    async fn copies_every_byte_then_half_closes() {
        let (mut source, from) = pair().await;
        let (to, mut sink) = pair().await;
        // Far more than the pipe and the socket buffers hold at once.
        let payload: Vec<u8> = (0..8 * 1024 * 1024).map(|i: u32| (i % 251) as u8).collect();
        let expected = payload.clone();

        let writer = tokio::spawn(async move {
            source.write_all(&payload).await.unwrap();
            source.shutdown().await.unwrap();
            source
        });
        let reader = tokio::spawn(async move {
            let mut received = Vec::new();
            sink.read_to_end(&mut received).await.unwrap();
            (received, sink)
        });

        let counted = AtomicU64::new(0);
        let total = copy(&from, &to, None, None, |moved| {
            counted.fetch_add(moved, Ordering::Relaxed);
        })
        .await
        .unwrap();

        // `read_to_end` returning proves the write side of `to` was shut down.
        let (received, mut sink) = reader.await.unwrap();
        assert_eq!(total, expected.len() as u64);
        assert_eq!(counted.load(Ordering::Relaxed), total);
        assert!(received == expected, "the payload arrived altered");

        // Only one direction closed: the other still carries data.
        let mut source = writer.await.unwrap();
        sink.write_all(b"reply").await.unwrap();
        sink.shutdown().await.unwrap();
        assert_eq!(copy(&to, &from, None, None, |_| {}).await.unwrap(), 5);
        let mut reply = Vec::new();
        source.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"reply");
    }

    #[tokio::test]
    async fn silent_source_hits_the_read_limit() {
        let (_source, from) = pair().await;
        let (to, _sink) = pair().await;
        let limit = Some(Duration::from_millis(50));
        let err = copy(&from, &to, limit, None, |_| {}).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn stalled_sink_hits_the_write_limit() {
        let (mut source, from) = pair().await;
        // `_sink` never reads, so the buffers of `to` fill up.
        let (to, _sink) = pair().await;
        let flood = tokio::spawn(async move {
            let chunk = vec![0; 64 * 1024];
            while source.write_all(&chunk).await.is_ok() {}
        });
        let limit = Some(Duration::from_millis(100));
        let err = copy(&from, &to, None, limit, |_| {}).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        flood.abort();
    }

    #[tokio::test]
    async fn reset_peer_is_an_error() {
        let (source, from) = pair().await;
        let (to, _sink) = pair().await;
        // Linger 0 turns the close into a reset.
        source.set_zero_linger().unwrap();
        drop(source);
        let err = copy(&from, &to, None, None, |_| {}).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionReset);
    }
}
