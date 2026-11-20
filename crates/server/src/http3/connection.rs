//! One QUIC connection: limits, handshake, requests, drain.

use std::future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use bytes::{Buf, Bytes};
use h3::error::{Code, ConnectionError, StreamError};
use h3::quic::StreamId;
use h3::server::{RequestResolver, RequestStream};
use http::header::{self, HeaderName};
use http::{HeaderMap, HeaderValue, Request, Response, StatusCode, Version};
use http_body_util::BodyExt;
use hyper::body::{Frame, SizeHint};
use quinn::{ConnectionError as QuicError, Incoming};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_rustls::rustls::pki_types::CertificateDer;

use super::listener::{self, Listener, Phase};
use crate::http::body::{self, Body, BodyError};
use crate::http::response::{self, BoxResponse};
use crate::http::service::{Activity, Service};
use crate::server::{self, PerIpSlot};
use crate::tls;

/// Connection-specific fields, which HTTP/3 forbids (RFC 9114, section
/// 4.2), removed from responses.
const CONNECTION_FIELDS: [HeaderName; 5] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// The `max_ack_delay` QUIC peers announce by default (RFC 9000, section
/// 18.2), part of the probe timeout.
const MAX_ACK_DELAY: Duration = Duration::from_millis(25);

/// Request stream of an HTTP/3 connection, or one of its halves.
type Stream<S> = RequestStream<S, Bytes>;

/// HTTP/3 side of a QUIC connection.
type Connection = h3::server::Connection<h3_quinn::Connection, Bytes>;

/// Request stream accepted, its head not read yet.
type Resolver = RequestResolver<h3_quinn::Connection, Bytes>;

/// Serves the requests of one QUIC connection until it closes, under the
/// limits of `listener`. A connection over a limit is refused, not queued:
/// the client falls back to TCP. No wait in here outlives
/// [`Phase::Closed`].
pub(super) async fn serve_connection(incoming: Incoming, listener: Arc<Listener>) {
    let context = &listener.context;
    let peer = incoming.remote_address();
    // Refused without a log line: a client over its limit could flood it.
    if *listener.phase.borrow() != Phase::Serving {
        incoming.refuse();
        return;
    }
    let validated = incoming.remote_address_validated();
    let available = listener.permits.available_permits();
    if needs_retry(
        available,
        listener.max_connections,
        context.per_ip.is_some(),
        validated,
        incoming.may_retry(),
    ) {
        // An error hands the attempt back, and dropping it refuses it.
        let _ = incoming.retry();
        return;
    }
    let Ok(_permit) = Arc::clone(&listener.permits).try_acquire_owned() else {
        incoming.refuse();
        return;
    };
    let _slot = match &context.per_ip {
        Some((max, counts)) => match PerIpSlot::acquire(*max, counts, peer.ip()) {
            Some(slot) => Some(slot),
            None => {
                incoming.refuse();
                return;
            }
        },
        None => None,
    };
    let _open = listener.count_open();
    let mut phase = listener.phase.subscribe();

    let log_name = &context.log.log_name;
    let deadline = context
        .read_header_timeout
        .unwrap_or(server::DEFAULT_HANDSHAKE_TIMEOUT);
    let handshake = tokio::select! {
        handshake = tokio::time::timeout(deadline, incoming) => handshake,
        _ = phase.wait_for(|phase| *phase == Phase::Closed) => return,
    };
    let quic = match handshake {
        Ok(Ok(connection)) => connection,
        // The endpoint was closed: the daemon stops.
        Ok(Err(QuicError::LocallyClosed)) => return,
        Ok(Err(err)) => {
            eprintln!("{log_name} => TLS handshake with {peer} failed: {err}");
            return;
        }
        Err(_) => {
            eprintln!("{log_name} => TLS handshake with {peer} timed out");
            return;
        }
    };
    let client_subject = quic
        .peer_identity()
        .and_then(|identity| identity.downcast::<Vec<CertificateDer<'static>>>().ok())
        .and_then(|chain| tls::client_subject(&chain));
    let service = context
        .http_service(peer, listener.address, client_subject, None)
        .expect("HTTP/3 listeners belong to HTTP servers");

    // h3 buffers a whole HEADERS frame before it decodes it and checks its
    // size limit, and opens its own streams while it builds the
    // connection, which a client that grants no stream credit can stall.
    // Both are bounded by time: the first request head and the build by
    // `read_header_timeout`, later heads by `idle_timeout`.
    let activity = Arc::clone(&service.activity);
    let first_request = context
        .read_header_timeout
        .unwrap_or(server::DEFAULT_READ_HEADER_TIMEOUT);
    let first_head = tokio::time::timeout(first_request, activity.first_request());
    let mut first_head = std::pin::pin!(first_head);
    let mut builder = h3::server::builder();
    builder.max_field_section_size(u64::from(server::MAX_HEADER_LIST_SIZE));
    let built = tokio::select! {
        built = builder.build(h3_quinn::Connection::new(quic.clone())) => Some(built),
        // No request can come before the connection is built.
        _ = &mut first_head => None,
        _ = phase.wait_for(|phase| *phase != Phase::Serving) => None,
    };
    let mut connection = match built {
        Some(Ok(connection)) => connection,
        Some(Err(err)) => {
            eprintln!("{log_name} => HTTP/3 connection from {peer}: {err}");
            return;
        }
        None => {
            quic.close(listener::no_error(), b"");
            return;
        }
    };

    let mut waiting_for_head = true;
    // The ID this side's GOAWAY carries, once sent.
    let mut goaway = None;
    let mut received_goaway = false;
    let mut accepted = Accepted::default();
    // Dropped, and so aborted, when the connection ends: no response can
    // reach the client any more.
    let mut requests = JoinSet::new();
    let idle = idle(context.idle_timeout, &activity);
    let mut idle = std::pin::pin!(idle);
    // True once every request was answered after a GOAWAY, false to close
    // at once.
    let answered = loop {
        let current = *phase.borrow_and_update();
        match current {
            Phase::Serving => {}
            Phase::Draining => {
                if goaway.is_none() {
                    tokio::select! {
                        id = accepted.send_goaway(&mut connection) => goaway = Some(id),
                        _ = phase.wait_for(|phase| *phase == Phase::Closed) => break false,
                    }
                }
            }
            Phase::Closed => break false,
        }
        if (goaway.is_some() || received_goaway) && requests.is_empty() {
            break true;
        }
        tokio::select! {
            // Still polled after this side's GOAWAY: h3 rejects the streams
            // above its ID, this loop the one at it.
            next = connection.accept(), if !received_goaway => match next {
                Ok(Some(resolver)) => {
                    let id = resolver.frame_stream.id();
                    if goaway.is_some_and(|goaway| id >= goaway) {
                        reject(resolver);
                    } else {
                        accepted.add(id);
                        requests.spawn(serve_request(resolver, service.clone()));
                    }
                }
                // The client sent GOAWAY: answer what it already asked for.
                Ok(None) => received_goaway = true,
                Err(err) => {
                    // Closes and timeouts from the client side are routine.
                    if matches!(err, ConnectionError::Local { .. }) && !err.is_h3_no_error() {
                        eprintln!("{log_name} => HTTP/3 connection from {peer}: {err}");
                    }
                    return;
                }
            },
            Some(_) = requests.join_next() => {}
            head = &mut first_head, if waiting_for_head => {
                if head.is_err() {
                    break false;
                }
                waiting_for_head = false;
            }
            // `idle_timeout` with no request in flight: GOAWAY, so the
            // client knows nothing it sent was dropped unseen, then close.
            () = &mut idle => {
                if goaway.is_none() {
                    tokio::select! {
                        _ = accepted.send_goaway(&mut connection) => {}
                        _ = phase.wait_for(|phase| *phase == Phase::Closed) => break false,
                    }
                }
                break true;
            }
            // The sender lives in `listener`: this cannot fail.
            _ = phase.changed() => {}
        }
    };
    drop(requests);
    if answered {
        tokio::select! {
            () = settled(&quic) => {}
            _ = phase.wait_for(|phase| *phase == Phase::Closed) => {}
        }
    }
    quic.close(listener::no_error(), b"");
}

/// Whether an incoming connection must prove its address with a Retry
/// before it takes one of `max` connection slots, `available` of them
/// free, and with `per_ip` one of the slots of its address. Spoofed
/// Initial packets cost no return path, and each one would hold a slot,
/// shared with TCP, for up to the handshake deadline: a spoofed address
/// could lock its owner out. Without `per_ip`, the extra round trip is
/// only paid when slots are scarce.
fn needs_retry(
    available: usize,
    max: usize,
    per_ip: bool,
    validated: bool,
    may_retry: bool,
) -> bool {
    // ponytail: fixed at half the slots; make it a setting if someone needs
    // another threshold.
    !validated && may_retry && (per_ip || available * 2 < max)
}

/// Request streams a connection accepted, for the ID of its GOAWAY.
#[derive(Default)]
struct Accepted {
    /// The last one accepted, which h3 counts the GOAWAY ID from.
    latest: Option<StreamId>,
    /// The highest one: streams can arrive out of order.
    highest: Option<StreamId>,
}

impl Accepted {
    fn add(&mut self, id: StreamId) {
        self.latest = Some(id);
        self.highest = self.highest.max(Some(id));
    }

    /// Sends GOAWAY with the ID after the highest stream accepted, so the
    /// client knows that every request it sent before is being answered
    /// and that the later ones were not processed and can be sent again
    /// (RFC 9114, section 5.2). Returns that ID. An error means the
    /// connection is gone, which the next `accept` reports.
    async fn send_goaway(&self, connection: &mut Connection) -> StreamId {
        let (Some(latest), Some(highest)) = (self.latest, self.highest) else {
            // h3 sends the first request stream ID.
            let _ = connection.shutdown(0).await;
            return StreamId::try_from(0).expect("0 is a request stream ID");
        };
        // h3 sends the ID `steps` request streams after `latest`.
        let steps = usize::try_from(highest.index() - latest.index() + 1)
            .expect("stream indexes are small");
        let _ = connection.shutdown(steps).await;
        latest + steps
    }
}

/// Refuses a request stream that came after this side's GOAWAY: the client
/// may send it again on a new connection.
fn reject(resolver: Resolver) {
    use h3::quic::{RecvStream, SendStream};

    let code = Code::H3_REQUEST_REJECTED.value();
    let mut stream = resolver.frame_stream.into_inner();
    stream.stop_sending(code);
    stream.reset(code);
}

/// Completes once the connection had no request in flight for `limit`, its
/// `idle_timeout`, as over TCP. Never without a limit.
async fn idle(limit: Option<Duration>, activity: &Activity) {
    let Some(limit) = limit else {
        return future::pending().await;
    };
    // ponytail: checks once per `limit`, so an idle connection closes
    // between one and two limits after its last response: `Activity` has
    // no event for the end of a request. Add one if that matters.
    loop {
        let requests = activity.requests.load(Ordering::Acquire);
        let was_idle = activity.in_flight() == 0;
        tokio::time::sleep(limit).await;
        if was_idle
            && activity.in_flight() == 0
            && activity.requests.load(Ordering::Acquire) == requests
        {
            return;
        }
    }
}

/// Longest wait in [`settled`], however busy the connection is.
const SETTLE_CAP: Duration = Duration::from_secs(3);

/// Resolves once the client closed the connection, or once its last
/// responses are acknowledged: closing earlier would drop the stream data
/// in flight.
async fn settled(quic: &quinn::Connection) {
    // ponytail: h3 0.0.8 hides `quinn::SendStream::stopped()`, which reports
    // the acknowledgement, and test or browser clients may keep the
    // connection open after GOAWAY. So the data counts as acknowledged once
    // the connection sends nothing for two probe timeouts: unacknowledged
    // data is sent again within one. Wait on `stopped()` once h3 exposes it.
    // A client that sends PINGs more often than that window keeps the server
    // sending ACKs, so the wait is also capped at `SETTLE_CAP`.
    let quiet = async {
        loop {
            let sent = quic.stats().udp_tx.datagrams;
            let probe_timeout = 3 * quic.rtt() + MAX_ACK_DELAY;
            tokio::time::sleep(2 * probe_timeout).await;
            if quic.stats().udp_tx.datagrams == sent {
                return;
            }
        }
    };
    let settled = async {
        tokio::select! {
            // Also on the idle timeout.
            _ = quic.closed() => {}
            () = quiet => {}
        }
    };
    let _ = tokio::time::timeout(SETTLE_CAP, settled).await;
}

/// Sending half of a request stream. Dropped before its response ended,
/// by a panic or an error, it resets the stream with `H3_INTERNAL_ERROR`:
/// quinn would end it cleanly and a cut response would look whole.
struct ResponseStream {
    stream: Stream<h3_quinn::SendStream<Bytes>>,
    finished: bool,
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        if !self.finished {
            self.stream.stop_stream(Code::H3_INTERNAL_ERROR);
        }
    }
}

/// Answers the request of one stream with `service`.
async fn serve_request(resolver: Resolver, service: Service) {
    // A stream reset before its head is complete has nothing to answer.
    let Ok((request, stream)) = resolver.resolve_request().await else {
        return;
    };
    let (send, receive) = stream.split();
    let mut send = ResponseStream {
        stream: send,
        finished: false,
    };
    let (mut parts, ()) = request.into_parts();
    parts.version = Version::HTTP_3;
    // h3 does not check `Content-Length`, and an HTTP/1.1 backend would
    // read a wrong one as the end of this request inside the next one.
    let Ok(declared) = content_length(&parts.headers) else {
        let bad_request = response::plain(StatusCode::BAD_REQUEST, "Bad Request");
        let _ = send_response(send, bad_request).await;
        return;
    };
    if let Some(length) = declared {
        // One value, also for repeated ones.
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    let (reset_sender, reset) = oneshot::channel();
    let (malformed_sender, mut malformed) = oneshot::channel();
    let body = request_body(receive, declared, reset_sender, malformed_sender).await;
    let response = tokio::select! {
        // `respond` first: it counts the request on its first poll.
        biased;
        response = service.respond(Request::from_parts(parts, body)) => response,
        // The client reset the request stream: dropping the response future
        // cancels the backend request too.
        // ponytail: a reset after the request stream ended (a `GET` whose end
        // came with its head) is not seen: h3 0.0.8 hides the
        // `quinn::SendStream::stopped()` that reports STOP_SENDING, so the
        // backend request runs until it answers. Race `stopped()` here once
        // an h3 release exposes it.
        Ok(()) = reset => return,
    };
    // The body did not match its `Content-Length`, so the backend request
    // failed: the request is malformed (RFC 9114, section 4.1.2).
    if malformed.try_recv().is_ok() {
        send.stream.stop_stream(Code::H3_MESSAGE_ERROR);
        send.finished = true;
        return;
    }
    // An error means the client reset the stream: it wants no answer.
    let _ = send_response(send, response).await;
}

/// Reads a request body from its stream: data frames, then trailers.
struct BodyReader {
    stream: Stream<h3_quinn::RecvStream>,
    /// Set once the data frames are over.
    trailers: bool,
    done: bool,
}

/// Next item of a [`BodyReader`].
type Next = Option<Result<Frame<Bytes>, StreamError>>;

impl BodyReader {
    fn poll_frame(&mut self, context: &mut Context<'_>) -> Poll<Next> {
        if self.done {
            return Poll::Ready(None);
        }
        if !self.trailers {
            match ready!(self.stream.poll_recv_data(context)) {
                Ok(Some(mut data)) => {
                    let data = data.copy_to_bytes(data.remaining());
                    return Poll::Ready(Some(Ok(Frame::data(data))));
                }
                Ok(None) => self.trailers = true,
                Err(err) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(err)));
                }
            }
        }
        let trailers = ready!(self.stream.poll_recv_trailers(context));
        self.done = true;
        Poll::Ready(
            trailers
                .transpose()
                .map(|trailers| trailers.map(Frame::trailers)),
        )
    }
}

/// A `Content-Length` that is not one valid length.
#[derive(Debug, PartialEq, Eq)]
struct InvalidLength;

/// Length a request declares in `Content-Length`, `None` without one.
/// Repeated values, in several fields or in a list, count once as they are
/// the same (RFC 9110, section 8.6). Only ASCII digits: Rust's integer
/// parser would also take a leading `+`.
fn content_length(headers: &HeaderMap) -> Result<Option<u64>, InvalidLength> {
    let mut length = None;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        let value = value.to_str().map_err(|_| InvalidLength)?;
        for item in value.split(',') {
            let item = item.trim_matches([' ', '\t']);
            if item.is_empty() || !item.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(InvalidLength);
            }
            let item: u64 = item.parse().map_err(|_| InvalidLength)?;
            if length.is_some_and(|length| length != item) {
                return Err(InvalidLength);
            }
            length = Some(item);
        }
    }
    Ok(length)
}

fn body_error(err: StreamError) -> BodyError {
    BodyError::Io(io::Error::other(err))
}

/// Body of the request whose stream `stream` is the receiving half of, and
/// whose `Content-Length` is `declared`. A request that already ended, as
/// most `GET`s do, gets an empty body, so it goes upstream without one,
/// unless it declares a length. Any other body is read by [`read_body`] and
/// handed over through a channel: the stream itself is not `Sync`. `reset`
/// is told if the client resets the stream before it ends, `malformed` if
/// the body does not match `declared`.
async fn request_body(
    stream: Stream<h3_quinn::RecvStream>,
    declared: Option<u64>,
    reset: oneshot::Sender<()>,
    malformed: oneshot::Sender<()>,
) -> Body {
    let mut reader = BodyReader {
        stream,
        trailers: false,
        done: false,
    };
    let first = future::poll_fn(|context| Poll::Ready(reader.poll_frame(context))).await;
    let first = match first {
        Poll::Ready(None) if declared.unwrap_or(0) == 0 => return body::empty(),
        Poll::Ready(next) => Some(next),
        Poll::Pending => None,
    };

    // A `GET` without `Content-Length` whose end of stream comes after
    // this peek goes upstream chunked: `is_end_stream` can only report an
    // end that already happened when the proxy sends the head.
    let (sender, receiver) = mpsc::channel(1);
    tokio::task::spawn(read_body(reader, first, sender, reset));
    let received = Received {
        frames: receiver,
        remaining: declared,
        last: None,
        malformed: Some(malformed),
    };
    received.boxed()
}

/// Hands the frames of `reader` to the body behind `sender`, starting with
/// `first` if it was already read. Tells `reset` when the client resets the
/// stream. Once the body is dropped (a backend can answer without it), the
/// rest of the stream is read and discarded, only to notice a reset, until
/// the request is answered.
async fn read_body(
    mut reader: BodyReader,
    mut first: Option<Next>,
    sender: mpsc::Sender<Result<Frame<Bytes>, BodyError>>,
    mut reset: oneshot::Sender<()>,
) {
    loop {
        let next = match first.take() {
            Some(next) => next,
            None => tokio::select! {
                next = future::poll_fn(|context| reader.poll_frame(context)) => next,
                // Neither a body nor a request is waiting for the stream.
                () = async {
                    sender.closed().await;
                    reset.closed().await;
                } => return,
            },
        };
        match next {
            None => return,
            Some(Ok(frame)) => {
                // A reset is not seen while this waits for room in the
                // channel, which only happens while the proxy holds the body
                // without reading it. An error means the body was dropped:
                // keep watching.
                let _ = sender.send(Ok(frame)).await;
            }
            Some(Err(err)) => {
                if let StreamError::RemoteTerminate { .. } = err {
                    let _ = reset.send(());
                }
                let _ = sender.send(Err(body_error(err))).await;
                return;
            }
        }
    }
}

/// Request body fed by the task that reads its stream. With a declared
/// length, a body that ends short of it or goes past it fails, which aborts
/// the backend request and closes its connection.
struct Received {
    frames: mpsc::Receiver<Result<Frame<Bytes>, BodyError>>,
    /// Bytes the `Content-Length` announces and that did not come yet.
    remaining: Option<u64>,
    /// The data that completes the declared length, held back until the
    /// stream ends: data after it would make the request malformed.
    last: Option<Bytes>,
    /// Told when the body does not match its declared length.
    malformed: Option<oneshot::Sender<()>>,
}

impl Received {
    /// What follows the end of the data: the data held back, or the end.
    fn end(&mut self) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        match self.last.take() {
            Some(last) => Poll::Ready(Some(Ok(Frame::data(last)))),
            None if self.remaining == Some(0) => Poll::Ready(None),
            None => self.mismatch("shorter"),
        }
    }

    fn mismatch(&mut self, comparison: &str) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        if let Some(malformed) = self.malformed.take() {
            let _ = malformed.send(());
        }
        let message = format!("request body {comparison} than its Content-Length");
        let err = io::Error::new(io::ErrorKind::InvalidData, message);
        Poll::Ready(Some(Err(BodyError::Io(err))))
    }

    /// Declared bytes not handed on yet.
    fn undelivered(&self) -> Option<u64> {
        let held = self.last.as_ref().map_or(0, |last| last.len() as u64);
        self.remaining.map(|remaining| remaining + held)
    }
}

impl hyper::body::Body for Received {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BodyError>>> {
        let this = self.get_mut();
        loop {
            let Some(remaining) = this.remaining else {
                return this.frames.poll_recv(context);
            };
            let data = match ready!(this.frames.poll_recv(context)) {
                Some(Ok(frame)) => match frame.into_data() {
                    Ok(data) => data,
                    // Trailers end the data. They are dropped: the head
                    // declared the length, not trailers.
                    Err(_) => return this.end(),
                },
                None => return this.end(),
                Some(Err(err)) => return Poll::Ready(Some(Err(err))),
            };
            let length = data.len() as u64;
            if length == 0 {
                continue;
            }
            if this.last.is_some() || length > remaining {
                return this.mismatch("longer");
            }
            this.remaining = Some(remaining - length);
            if length == remaining {
                this.last = Some(data);
                continue;
            }
            return Poll::Ready(Some(Ok(Frame::data(data))));
        }
    }

    /// Ended once the declared length was handed on, or once the reader
    /// task delivered everything.
    fn is_end_stream(&self) -> bool {
        match self.undelivered() {
            Some(undelivered) => undelivered == 0,
            None => self.frames.is_closed() && self.frames.is_empty(),
        }
    }

    /// Exact when the request declares its length.
    fn size_hint(&self) -> SizeHint {
        self.undelivered()
            .map_or_else(SizeHint::default, SizeHint::with_exact)
    }
}

/// Sends `response` on `send`: head, data, trailers, end. A body that fails
/// leaves the response unfinished, so the stream is reset and the client
/// cannot take a cut body for a whole one.
async fn send_response(mut send: ResponseStream, response: BoxResponse) -> Result<(), StreamError> {
    let stream = &mut send.stream;
    let (mut parts, mut body) = response.into_parts();
    for name in &CONNECTION_FIELDS {
        parts.headers.remove(name);
    }
    stream
        .send_response(Response::from_parts(parts, ()))
        .await?;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            return Ok(());
        };
        match frame.into_data() {
            Ok(data) => stream.send_data(data).await?,
            Err(frame) => {
                if let Ok(trailers) = frame.into_trailers() {
                    stream.send_trailers(trailers).await?;
                }
            }
        }
    }
    let finished = stream.finish().await;
    // Resetting now could discard what was sent and not yet acknowledged.
    send.finished = true;
    finished
}

#[cfg(test)]
mod tests {
    use http::HeaderValue;

    use super::*;

    fn length_of(values: &[&'static str]) -> Result<Option<u64>, InvalidLength> {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(header::CONTENT_LENGTH, HeaderValue::from_static(value));
        }
        content_length(&headers)
    }

    #[test]
    fn content_length_values() {
        assert_eq!(length_of(&[]), Ok(None));
        assert_eq!(length_of(&["5"]), Ok(Some(5)));
        assert_eq!(length_of(&["5", "5"]), Ok(Some(5)));
        assert_eq!(length_of(&["5, 5"]), Ok(Some(5)));
        for invalid in [
            &["+5"][..],
            &["-5"],
            &[""],
            &["abc"],
            &["5, 6"],
            &["5", "6"],
            &["5,"],
            &["99999999999999999999"],
        ] {
            assert_eq!(length_of(invalid), Err(InvalidLength), "{invalid:?}");
        }
    }

    #[test]
    fn retry_when_fewer_than_half_the_slots_are_free() {
        // Half or more of 4 slots free: no extra round trip.
        assert!(!needs_retry(2, 4, false, false, true));
        // Fewer than half free.
        assert!(needs_retry(1, 4, false, false, true));
        assert!(needs_retry(0, 4, false, false, true));
        // A validated address, or a client that already answered a Retry.
        assert!(!needs_retry(0, 4, false, true, true));
        assert!(!needs_retry(0, 4, false, false, false));
    }

    #[test]
    fn retry_always_with_max_connections_per_ip() {
        // Slots to spare, but each unvalidated client would take one of
        // the slots of the address it claims.
        assert!(needs_retry(4, 4, true, false, true));
        assert!(!needs_retry(4, 4, true, true, true));
        assert!(!needs_retry(4, 4, true, false, false));
    }
}
