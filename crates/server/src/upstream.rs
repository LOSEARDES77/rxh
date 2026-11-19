//! Upstream selection with retries, shared by the HTTP and TCP proxies.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use rxh_core::pool::{Pool, RequestGuard, RetryPermit, SelectContext, Selection, Upstream};

/// Why no upstream connection could be made.
#[derive(Debug)]
pub(crate) enum DialError {
    /// The pool has no available member (all down, full or empty), or the
    /// request waited `queue_timeout` for one below `max_connections`.
    NoUpstream,

    /// The last attempt failed to connect.
    Connect(io::Error),
}

/// Selects an upstream and connects to it, retrying according to the pool's
/// `retries`, `try_duration` and `try_interval` settings. Connection failures
/// are reported to passive health checks. The request counts as in flight
/// on the backend, within `max_connections`, until the guard is dropped.
/// A retry also needs room in the `retry_budget` of the pool; `retry_permit`
/// then counts the request as retrying until the caller drops it.
pub(crate) async fn connect_with_retries<T, F, Fut>(
    pool: &Pool,
    context: &SelectContext<'_>,
    retry_permit: &mut Option<RetryPermit>,
    mut connect: F,
) -> Result<(Selection, RequestGuard, T), DialError>
where
    F: FnMut(Arc<Upstream>) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let settings = pool.settings();
    let started = Instant::now();
    let mut attempts = 0;

    loop {
        let Ok(acquired) =
            tokio::time::timeout(settings.queue_timeout, pool.acquire(context)).await
        else {
            eprintln!(
                "{} => no backend below max_connections within {:?}",
                pool.name(),
                settings.queue_timeout
            );
            return Err(DialError::NoUpstream);
        };
        let error = match acquired {
            Some((selection, guard)) => match connect(Arc::clone(&selection.upstream)).await {
                Ok(connection) => return Ok((selection, guard, connection)),
                Err(err) => {
                    selection.upstream.record_failure();
                    eprintln!(
                        "{} => dial {} failed: {err}",
                        pool.name(),
                        selection.upstream.address()
                    );
                    DialError::Connect(err)
                }
            },
            None => DialError::NoUpstream,
        };

        attempts += 1;
        let retry = match settings.try_duration {
            Some(limit) => started.elapsed() + settings.try_interval < limit,
            None => attempts <= settings.retries,
        };
        if !retry || !pool.allow_retry(retry_permit) {
            return Err(error);
        }

        tokio::time::sleep(settings.try_interval).await;
    }
}
