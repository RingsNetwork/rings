//! `tcp`: a byte stream over an onion session, native only (#834 D1′, D2′).
//!
//! The exit half is the socket world (`OnionTcpWorld`): a session's target is resolved once to
//! its public addresses and connected, and its two halves are the session's world. The client
//! half is [`NativeOnionOpenStream`]: a session opened over a route, relayed against a local
//! byte stream (a CONNECT tunnel, a SOCKS stream, a captured gateway flow) by the shared duplex
//! pump.
//!
//! ```text
//! local ─read─▶ pump ─data─▶ session ═loops═▶ h ─write─▶ target
//! local ◀write─ pump ◀data── session ◀replies═ h ◀read── target    (only while h holds credit)
//! ```
//!
//! Law (pause): `h` reads its socket only while the session holds reply blocks, so an
//! unreplenished session pauses the target instead of dropping its bytes, and resumes on credit.
//!
//! Law (fail closed): a session that ends by `abort`, a gap, or any failure resets the local
//! stream ([`OnionLocalStream::reset`], an RST for a socket); only a pump that closed both halves
//! in order closes it cleanly, so a truncated stream is never presented as complete.

use std::time::Duration;

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio::time::Instant;

use crate::error::Error;
use crate::error::Result;
use crate::onion::session::dial::OnionClientStream;
use crate::onion::session::serve::OnionWorld;
use crate::onion::session::serve::OnionWorldReader;
use crate::onion::session::serve::OnionWorldWriter;
use crate::onion::target::resolve_public_target;
use crate::onion::OnionProxyTarget;

mod duplex;
mod pump;

use pump::pump_tcp_duplex;
use pump::OnionPumpEnd;

/// Largest read of the local stream, split into frames by the session.
const TCP_BUF: usize = 30_000;

/// Longest wait for a target to connect.
const TCP_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// The quantum a connect result is delayed to, so a client learns the resolver's and the
/// target's timing only to this resolution.
const TCP_OPEN_RESPONSE_QUANTUM_MS: u128 = 250;

/// The instant a connect result that took `now − opened_at` is released at: the next multiple
/// of the quantum after `opened_at`, at least one quantum.
fn open_response_deadline(opened_at: Instant, now: Instant) -> Instant {
    let elapsed_ms = now.saturating_duration_since(opened_at).as_millis();
    let quanta = elapsed_ms.div_ceil(TCP_OPEN_RESPONSE_QUANTUM_MS).max(1);
    let deadline_ms =
        u64::try_from(quanta.saturating_mul(TCP_OPEN_RESPONSE_QUANTUM_MS)).unwrap_or(u64::MAX);
    opened_at
        .checked_add(Duration::from_millis(deadline_ms))
        .unwrap_or(now)
}

/// Connect the first public address of `target` that answers.
async fn connect(target: &OnionProxyTarget) -> Result<TcpStream> {
    let addresses = resolve_public_target(target).await?;
    let mut last = None;
    for address in addresses {
        match TcpStream::connect(address).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(Error::HttpRequestError(format!(
        "onion TCP target {} did not connect: {last:?}",
        target.authority()
    )))
}

/// The socket world of a `tcp` exit.
pub(crate) struct OnionTcpWorld;

/// The read half of a connected target.
pub(crate) struct OnionTcpReader(OwnedReadHalf);

/// The write half of a connected target.
pub(crate) struct OnionTcpWriter(OwnedWriteHalf);

#[async_trait::async_trait]
impl OnionWorld for OnionTcpWorld {
    type Reader = OnionTcpReader;
    type Writer = OnionTcpWriter;

    /// Resolve and connect `target`, releasing the result, success or failure, at the next open
    /// quantum.
    async fn open(&self, target: &OnionProxyTarget) -> Result<(Self::Reader, Self::Writer)> {
        let opened_at = Instant::now();
        let connected = match timeout(TCP_OPEN_TIMEOUT, connect(target)).await {
            Ok(connected) => connected,
            Err(_) => Err(Error::OnionRouteError(
                crate::onion::OnionRouteError::TcpOpenTimedOut,
            )),
        };
        tokio::time::sleep_until(open_response_deadline(opened_at, Instant::now())).await;
        let (read, write) = connected?.into_split();
        Ok((OnionTcpReader(read), OnionTcpWriter(write)))
    }
}

#[async_trait::async_trait]
impl OnionWorldReader for OnionTcpReader {
    async fn read(&mut self, max: usize) -> Result<Option<Bytes>> {
        let mut buffer = vec![0; max.max(1)];
        let read =
            self.0.read(&mut buffer).await.map_err(|error| {
                Error::HttpRequestError(format!("onion TCP target read: {error}"))
            })?;
        if read == 0 {
            return Ok(None);
        }
        buffer.truncate(read);
        Ok(Some(Bytes::from(buffer)))
    }
}

#[async_trait::async_trait]
impl OnionWorldWriter for OnionTcpWriter {
    async fn write(&mut self, bytes: Bytes) -> Result<()> {
        self.0
            .write_all(&bytes)
            .await
            .map_err(|error| Error::HttpRequestError(format!("onion TCP target write: {error}")))
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.0
            .shutdown()
            .await
            .map_err(|error| Error::HttpRequestError(format!("onion TCP target shutdown: {error}")))
    }
}

/// A client `tcp` session whose exit has connected its target.
pub struct NativeOnionOpenStream {
    /// The open session.
    stream: OnionClientStream,
}

impl NativeOnionOpenStream {
    /// Wrap an opened session.
    pub(crate) const fn new(stream: OnionClientStream) -> Self {
        Self { stream }
    }

    /// Relay the local byte stream `local` through the session, until both directions close;
    /// a pump that does not close both halves in order resets `local` (see the module's
    /// fail-closed law).
    pub fn relay<S>(self, local: S)
    where S: OnionLocalStream {
        let (sender, receiver) = self.stream.split();
        tokio::spawn(async move {
            match pump_tcp_duplex(local, sender, receiver).await {
                (_, OnionPumpEnd::Closed) => {}
                (local, OnionPumpEnd::Failed) => local.reset(),
            }
        });
    }
}

/// A local byte stream a client `tcp` session is relayed against, with the effect that ends it
/// as failed.
///
/// Law: `reset` is observably distinct from a clean close to the stream's peer wherever the
/// stream's transport can express it, so a failed session is never read as a complete one.
pub trait OnionLocalStream:
    tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static
{
    /// End the stream as failed: never a clean shutdown.
    fn reset(self);
}

impl OnionLocalStream for TcpStream {
    /// An RST: `SO_LINGER = 0` makes the close abortive, discarding unsent bytes.
    fn reset(self) {
        if let Err(error) = self.set_zero_linger() {
            tracing::debug!(%error, "onion TCP local reset fell back to a close");
        }
    }
}

#[cfg(test)]
mod tests;
