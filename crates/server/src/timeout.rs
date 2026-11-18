//! Stream wrappers behind the timeout options: per operation limits for
//! upstream sockets ([`Timed`]) and shared activity tracking for idle
//! client connections ([`Watched`]).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::{Sleep, sleep};

/// Fails an operation that stays pending longer than `limit`.
#[derive(Debug, Default)]
struct Limit {
    limit: Option<Duration>,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl Limit {
    fn poll<T>(
        &mut self,
        cx: &mut Context<'_>,
        result: Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        let Some(limit) = self.limit else {
            return result;
        };
        if result.is_ready() {
            self.sleep = None;
            return result;
        }
        let timer = self.sleep.get_or_insert_with(|| Box::pin(sleep(limit)));
        match timer.as_mut().poll(cx) {
            Poll::Ready(()) => {
                self.sleep = None;
                Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "no progress")))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Stream whose reads and writes fail with `TimedOut` after making no
/// progress for the configured time.
#[derive(Debug)]
pub(crate) struct Timed<S> {
    inner: S,
    read: Limit,
    write: Limit,
}

impl<S> Timed<S> {
    pub(crate) fn new(inner: S, read: Option<Duration>, write: Option<Duration>) -> Self {
        Self {
            inner,
            read: Limit {
                limit: read,
                sleep: None,
            },
            write: Limit {
                limit: write,
                sleep: None,
            },
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Timed<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        this.read.poll(cx, result)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Timed<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        this.write.poll(cx, result)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        this.write.poll(cx, result)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Time of the last byte moved in either direction.
#[derive(Debug)]
pub(crate) struct Stamp {
    start: Instant,
    last_millis: AtomicU64,
}

impl Stamp {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            start: Instant::now(),
            last_millis: AtomicU64::new(0),
        })
    }

    fn touch(&self) {
        let millis = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_millis.store(millis, Ordering::Relaxed);
    }

    fn idle(&self) -> Duration {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(now.saturating_sub(self.last_millis.load(Ordering::Relaxed)))
    }

    /// Completes once nothing moved for `limit` while `busy` is false.
    pub(crate) async fn idle_for(&self, limit: Duration, busy: impl Fn() -> bool) {
        loop {
            let idle = self.idle();
            let busy = busy();
            if !busy && idle >= limit {
                return;
            }
            let wait = if busy {
                limit
            } else {
                limit.saturating_sub(idle)
            };
            sleep(wait.max(Duration::from_millis(5))).await;
        }
    }
}

/// Stream that records its traffic in a [`Stamp`].
#[derive(Debug)]
pub(crate) struct Watched<S> {
    inner: S,
    stamp: Arc<Stamp>,
}

impl<S> Watched<S> {
    pub(crate) fn new(inner: S, stamp: Arc<Stamp>) -> Self {
        Self { inner, stamp }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Watched<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if result.is_ready() {
            this.stamp.touch();
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Watched<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.inner).poll_write(cx, buf);
        if result.is_ready() {
            this.stamp.touch();
        }
        result
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
    use super::*;

    #[tokio::test]
    async fn idle_for_survives_busy_flipping() {
        let stamp = Stamp::new();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        // Busy on odd calls: the connection is already past the limit.
        tokio::time::sleep(Duration::from_millis(30)).await;
        stamp
            .idle_for(Duration::from_millis(10), || {
                calls.fetch_add(1, Ordering::SeqCst).is_multiple_of(2)
            })
            .await;
    }
}
