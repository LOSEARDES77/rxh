//! Body types shared by requests and responses.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Frame, SizeHint};

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
