//! Body types shared by requests and responses.

use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::{Buf, Bytes, BytesMut};
use http::HeaderMap;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Frame, SizeHint};
use tokio::io::{AsyncBufRead, AsyncRead, ReadBuf};

/// Error of a [`Body`].
// Not a `Box<dyn Error>`: rustc then rejects the service future as not `Send`.
#[derive(Debug)]
pub(crate) enum BodyError {
    /// The connection the body comes from failed.
    Hyper(hyper::Error),

    /// Reading a file or compressing failed.
    Io(std::io::Error),

    /// The request body is larger than `request.max_body_size`.
    TooLarge,
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hyper(err) => err.fmt(f),
            Self::Io(err) => err.fmt(f),
            Self::TooLarge => f.write_str("request body over max_body_size"),
        }
    }
}

impl std::error::Error for BodyError {}

/// Boxed body used for every request sent upstream and every response.
pub(crate) type Body = BoxBody<Bytes, BodyError>;

/// Boxes a body received by hyper.
pub(crate) fn incoming(body: hyper::body::Incoming) -> Body {
    body.map_err(BodyError::Hyper).boxed()
}

/// Reads `body` until it ends or `limit` bytes are held, and returns a body
/// that yields the bytes read followed by the rest. A body that fits has an
/// exact size, so it is sent with a `Content-Length`.
pub(crate) async fn buffer(mut body: Body, limit: usize) -> Result<Body, BodyError> {
    let mut data = BytesMut::new();
    let mut trailers = None;
    while data.len() < limit && !hyper::body::Body::is_end_stream(&body) {
        match body.frame().await.transpose()?.map(Frame::into_data) {
            Some(Ok(chunk)) => data.extend_from_slice(&chunk),
            Some(Err(frame)) => {
                // Trailers are the last frame.
                trailers = frame.into_trailers().ok();
                body = empty();
            }
            None => body = empty(),
        }
    }
    let buffered = Buffered {
        data: data.freeze(),
        trailers,
        rest: body,
    };
    Ok(buffered.boxed())
}

/// Body made by [`buffer`]: the buffered part, then the rest of the source.
struct Buffered {
    data: Bytes,
    trailers: Option<HeaderMap>,
    rest: Body,
}

impl hyper::body::Body for Buffered {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if !self.data.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(std::mem::take(&mut self.data)))));
        }
        if let Some(trailers) = self.trailers.take() {
            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }
        Pin::new(&mut self.rest).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_empty() && self.trailers.is_none() && self.rest.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.rest.size_hint();
        let buffered = self.data.len() as u64;
        // The upper bound first: `set_lower` asserts it stays below it.
        if let Some(upper) = hint.upper() {
            hint.set_upper(upper + buffered);
        }
        hint.set_lower(hint.lower() + buffered);
        hint
    }
}

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
    type Error = BodyError;

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

/// Body that streams everything an [`AsyncRead`] yields, in chunks.
pub(crate) struct ReaderBody<R> {
    reader: R,
    buf: Vec<u8>,
    source_error: Option<ErrorSlot>,
    label: String,
}

/// Where a [`BodyReader`] leaves the original error of the body it wraps.
pub(crate) type ErrorSlot = Arc<Mutex<Option<BodyError>>>;

impl<R> ReaderBody<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            buf: vec![0; CHUNK],
            source_error: None,
            label: String::new(),
        }
    }

    /// Returns the error found in `slot` when a read fails, instead of the
    /// read error.
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
    type Error = BodyError;

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
                Poll::Ready(Some(Err(source.unwrap_or_else(|| {
                    eprintln!("read error on {}: {err}", this.label);
                    BodyError::Io(err)
                }))))
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
