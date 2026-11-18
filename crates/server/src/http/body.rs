//! Body types shared by requests and responses.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::{Buf, Bytes};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Frame, SizeHint};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

/// Boxed body used for every request sent upstream and every response.
pub(crate) type Body = BoxBody<Bytes, hyper::Error>;

/// Single chunk body.
pub(crate) fn full(chunk: impl Into<Bytes>) -> Body {
    Full::new(chunk.into())
        .map_err(|never| match never {})
        .boxed()
}

/// Empty body.
pub(crate) fn empty() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// Body that keeps `guard` alive until the body is fully sent or dropped.
/// Used to count a request as in flight while its response streams.
pub(crate) struct Guarded<G> {
    inner: Body,
    guard: Option<G>,
}

impl<G> Guarded<G> {
    pub(crate) fn new(inner: Body, guard: G) -> Self {
        Self {
            inner,
            guard: Some(guard),
        }
    }
}

impl<G: Unpin> hyper::body::Body for Guarded<G> {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(context);
        if let Poll::Ready(None | Some(Err(_))) = poll {
            self.guard = None;
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// Size of the chunks [`ReaderBody`] reads.
const CHUNK: usize = 64 * 1024;

/// Body that streams everything an [`AsyncRead`] yields, in chunks. A read
/// error ends the body early because hyper errors cannot be built here.
pub(crate) struct ReaderBody<R> {
    reader: R,
    buf: Vec<u8>,
    source_error: Option<ErrorSlot>,
    label: String,
}

/// Where a [`BodyReader`] leaves the original error of the body it wraps.
pub(crate) type ErrorSlot = Arc<Mutex<Option<hyper::Error>>>;

impl<R> ReaderBody<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; CHUNK],
            source_error: None,
            label: String::new(),
        }
    }

    /// Returns the error found in `slot` when a read fails, instead of ending
    /// the body as if it were complete.
    pub(crate) fn with_source_error(mut self, slot: ErrorSlot) -> Self {
        self.source_error = Some(slot);
        self
    }

    /// Names the source in the message logged when a read fails.
    pub(crate) fn labeled(mut self, label: String) -> Self {
        self.label = label;
        self
    }
}

impl<R: AsyncRead + Unpin> hyper::body::Body for ReaderBody<R> {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let mut buf = ReadBuf::new(&mut this.buf);
        match Pin::new(&mut this.reader).poll_read(context, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(())) if !buf.filled().is_empty() => {
                Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(buf.filled())))))
            }
            Poll::Ready(Ok(())) => Poll::Ready(None),
            Poll::Ready(Err(err)) => {
                let source = this
                    .source_error
                    .as_ref()
                    .and_then(|slot| slot.lock().ok()?.take());
                match source {
                    Some(source) => Poll::Ready(Some(Err(source))),
                    None => {
                        // hyper errors cannot be built here: the connection is
                        // aborted when a length was promised, else the body ends.
                        eprintln!("read error on {}: {err}", this.label);
                        Poll::Ready(None)
                    }
                }
            }
        }
    }
}

/// Reads the data frames of a body as bytes. Trailers are dropped.
pub(crate) struct BodyReader {
    body: Body,
    chunk: Bytes,
    error: ErrorSlot,
}

impl BodyReader {
    pub(crate) fn new(body: Body) -> Self {
        Self {
            body,
            chunk: Bytes::new(),
            error: ErrorSlot::default(),
        }
    }

    /// Slot that receives the error of the wrapped body.
    pub(crate) fn error_slot(&self) -> ErrorSlot {
        Arc::clone(&self.error)
    }
}

impl AsyncRead for BodyReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let chunk = match self.as_mut().poll_fill_buf(context) {
            Poll::Ready(Ok(chunk)) => chunk,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        };
        let amount = chunk.len().min(buf.remaining());
        buf.put_slice(&chunk[..amount]);
        self.consume(amount);
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for BodyReader {
    fn poll_fill_buf(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<&[u8]>> {
        let this = self.get_mut();
        while this.chunk.is_empty() {
            match hyper::body::Body::poll_frame(Pin::new(&mut this.body), context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => break,
                Poll::Ready(Some(Err(err))) => {
                    let failure = std::io::Error::other(err.to_string());
                    if let Ok(mut slot) = this.error.lock() {
                        *slot = Some(err);
                    }
                    return Poll::Ready(Err(failure));
                }
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        this.chunk = data;
                    }
                }
            }
        }
        Poll::Ready(Ok(&this.chunk))
    }

    fn consume(self: Pin<&mut Self>, amount: usize) {
        self.get_mut().chunk.advance(amount);
    }
}
