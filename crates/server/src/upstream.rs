//! Upstream selection with retries, shared by the HTTP and TCP proxies.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rxh_core::pool::{Pool, SelectContext, Selection, Upstream};
use tokio::net::TcpStream;

/// Why no upstream connection could be made.
#[derive(Debug)]
pub(crate) enum DialError {
    /// The pool has no available member (all down, full or empty).
    NoUpstream,

    /// The last attempt failed to connect.
    Connect(io::Error),
}

/// Selects an upstream and connects to it, retrying according to the pool's
/// `retries`, `try_duration` and `try_interval` settings. Connection failures
/// are reported to passive health checks.
pub(crate) async fn connect_with_retries<T, F, Fut>(
    pool: &Pool,
    context: &SelectContext<'_>,
    mut connect: F,
) -> Result<(Selection, T), DialError>
where
    F: FnMut(Arc<Upstream>) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let settings = pool.settings();
    let started = Instant::now();
    let mut attempts = 0;

    loop {
        let error = match pool.select(context) {
            Some(selection) => match connect(Arc::clone(&selection.upstream)).await {
                Ok(connection) => return Ok((selection, connection)),
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
        if !retry {
            return Err(error);
        }

        tokio::time::sleep(settings.try_interval).await;
    }
}

/// Opens a TCP connection with a timeout and `TCP_NODELAY`.
pub(crate) async fn dial(address: &str, timeout: Duration) -> io::Result<TcpStream> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(address))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "dial timeout"))??;
    stream.set_nodelay(true)?;
    Ok(stream)
}
