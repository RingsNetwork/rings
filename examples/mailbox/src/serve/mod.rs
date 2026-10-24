//! The accept loop every listener of the mailbox runs: one task per connection.
//!
//! A failed accept (out of descriptors, say) pauses the loop, twice as long on each further
//! failure up to a bound, so the loop never spins; a success resets the pause. Finished
//! connections are reaped as they end, so the loop's task set does not grow with every one.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::task::JoinSet;

/// First pause after a failed accept.
const BACKOFF_FIRST: Duration = Duration::from_millis(10);
/// Longest pause after a failed accept.
const BACKOFF_MAX: Duration = Duration::from_secs(1);

/// The pause after a failed accept that followed a pause of `previous`.
pub fn next_backoff(previous: Duration) -> Duration {
    previous.saturating_mul(2).min(BACKOFF_MAX)
}

/// Accept on `listener` forever, running `connection` on each accepted stream in its own task;
/// `name` labels the loop's log lines.
pub async fn serve_each<C, F>(listener: TcpListener, name: &'static str, connection: C)
where
    C: Fn(TcpStream, SocketAddr) -> F,
    F: Future<Output = ()> + Send + 'static,
{
    let mut connections = JoinSet::new();
    let mut backoff = BACKOFF_FIRST;
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    backoff = BACKOFF_FIRST;
                    connections.spawn(connection(stream, peer));
                }
                Err(error) => {
                    tracing::warn!("{name}: accept failed: {error}");
                    tokio::time::sleep(backoff).await;
                    backoff = next_backoff(backoff);
                }
            },
            Some(_finished) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

#[cfg(test)]
mod test_serve;
