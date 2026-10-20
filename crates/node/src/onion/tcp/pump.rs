//! The duplex pump of a client `tcp` session: one local byte stream against one session.
//!
//! ```text
//! loop select, until both halves close or the stream idles for its idle timeout, if it has one:
//!   local read  n > 0 ─▶ session.send(bytes)      local EOF ─▶ session.fin, close read
//!   session Data(w)   ─▶ local write(w)           session Fin ─▶ local shutdown, close write
//!   session Failed | end ─▶ stop
//! end = Closed  iff the session's fin reached the local stream (its write half shut in order)
//!     = Failed  otherwise                         ─▶ the caller resets the local stream
//! ```
//!
//! Laws: the halves close independently (TCP half-close); the local stream is read only while
//! its read half is open, and written only while its write half is. A stream whose incoming
//! bytes did not end in order ends [`OnionPumpEnd::Failed`]: a truncated stream is never
//! presented as complete (#843 D2′ `abort`). A failure after the session's `fin` (an upload
//! the exit could no longer write) ends [`OnionPumpEnd::Closed`]: the local stream already
//! holds every byte and an orderly end, and a reset would discard bytes still in its send
//! buffer, turning a complete stream into an error.

use std::future::pending;
use std::pin::Pin;
use std::time::Duration;

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;
use tokio::time::Instant;
use tokio::time::Sleep;

use super::duplex::TcpDuplexState;
use super::TCP_BUF;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::dial::OnionStreamReceiver;
use crate::onion::session::dial::OnionStreamSender;

/// How a pump ended, a function of its final [`TcpDuplexState`] alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnionPumpEnd {
    /// The session's `fin` reached the local stream: every incoming byte was written in order.
    Closed,
    /// The incoming stream did not end in order (the session failed before its `fin`, a side
    /// broke, or the stream idled out): the local stream must be reset, never closed cleanly.
    Failed,
}

impl OnionPumpEnd {
    /// The end of a pump whose halves stopped in `state`.
    const fn of(state: TcpDuplexState) -> Self {
        if state.delivered_whole() {
            Self::Closed
        } else {
            Self::Failed
        }
    }
}

/// A running idle timer: it fires `timeout` after the last traffic.
struct IdleTimer {
    /// The pending expiry.
    sleep: Pin<Box<Sleep>>,
    /// The period every traffic restarts it with.
    timeout: Duration,
}

/// The pump's idle clock: a timer, or none, which never expires.
struct IdleClock(Option<IdleTimer>);

impl IdleClock {
    /// A clock expiring `timeout` from now, or one that never expires for `None`.
    fn new(timeout: Option<Duration>) -> Self {
        Self(timeout.map(|timeout| IdleTimer {
            sleep: Box::pin(tokio::time::sleep(timeout)),
            timeout,
        }))
    }

    /// Traffic passed: the clock restarts.
    fn touch(&mut self) {
        if let Some(timer) = self.0.as_mut() {
            timer.sleep.as_mut().reset(Instant::now() + timer.timeout);
        }
    }

    /// Wait until the clock expires.
    async fn expired(&mut self) {
        match self.0.as_mut() {
            Some(timer) => timer.sleep.as_mut().await,
            None => pending().await,
        }
    }
}

/// Pump `local` against the session halves (see the module diagram) with the idle timeout
/// `idle_timeout`, or none, returning the local stream with the pump's end so the caller
/// applies the matching close.
pub(super) async fn pump_tcp_duplex<S>(
    local: S,
    sender: OnionStreamSender,
    receiver: OnionStreamReceiver,
    idle_timeout: Option<Duration>,
) -> (S, OnionPumpEnd)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(local);
    let mut state = TcpDuplexState::open();
    pump_halves(
        &mut read,
        &mut write,
        &mut state,
        sender,
        receiver,
        IdleClock::new(idle_timeout),
    )
    .await;
    (read.unsplit(write), OnionPumpEnd::of(state))
}

/// The select loop of [`pump_tcp_duplex`]: returns when both halves close, on the first
/// failure, or once `idle` expires without traffic, leaving the reached halves in `state`.
async fn pump_halves<S>(
    read: &mut ReadHalf<S>,
    write: &mut WriteHalf<S>,
    state: &mut TcpDuplexState,
    mut sender: OnionStreamSender,
    mut receiver: OnionStreamReceiver,
    mut idle: IdleClock,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; TCP_BUF];
    while !state.is_closed() {
        tokio::select! {
            read_result = read.read(buffer.as_mut_slice()), if state.can_read() => {
                idle.touch();
                match read_result {
                    Ok(0) => {
                        if sender.fin().await.is_err() {
                            return;
                        }
                        state.close_read();
                    }
                    Ok(n) => {
                        let Some(chunk) = buffer.get(..n) else {
                            return;
                        };
                        if sender.send(bytes::Bytes::copy_from_slice(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "onion TCP local read failed");
                        return;
                    }
                }
            }
            event = receiver.next() => {
                idle.touch();
                match event {
                    Some(OnionStreamEvent::Data(bytes)) => {
                        if state.can_write() && write.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                    Some(OnionStreamEvent::Fin) => {
                        if state.can_write() {
                            if write.shutdown().await.is_err() {
                                return;
                            }
                            state.close_write();
                        }
                    }
                    Some(OnionStreamEvent::Failed) | None => return,
                }
            }
            () = idle.expired() => return,
        }
    }
}
