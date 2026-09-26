//! The duplex pump of a client `tcp` session: one local byte stream against one session.
//!
//! ```text
//! loop select, until both halves close or the stream idles for RELAY_IDLE_TIMEOUT:
//!   local read  n > 0 ─▶ session.send(bytes)      local EOF ─▶ session.fin, close read
//!   session Data(w)   ─▶ local write(w)           session Fin ─▶ local shutdown, close write
//!   session Failed | end ─▶ stop
//! end = Closed  iff both halves closed in order
//!     = Failed  otherwise                         ─▶ the caller resets the local stream
//! ```
//!
//! Laws: the halves close independently (TCP half-close); the local stream is read only while
//! its read half is open, and written only while its write half is. A stream the pump did not
//! close in order ends [`OnionPumpEnd::Failed`]: a truncated stream is never presented as
//! complete (#843 D2′ `abort`).

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;

use super::duplex::TcpDuplexState;
use super::TCP_BUF;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::dial::OnionStreamReceiver;
use crate::onion::session::dial::OnionStreamSender;

/// How a pump ended, a function of its final [`TcpDuplexState`] alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OnionPumpEnd {
    /// Both halves closed in order: every byte of both directions was carried.
    Closed,
    /// The session failed, a side broke, or the stream idled out: the local stream must be
    /// reset, never closed cleanly.
    Failed,
}

impl OnionPumpEnd {
    /// The end of a pump whose halves stopped in `state`.
    const fn of(state: TcpDuplexState) -> Self {
        if state.is_closed() {
            Self::Closed
        } else {
            Self::Failed
        }
    }
}

/// Pump `local` against the session halves (see the module diagram), returning the local
/// stream with the pump's end so the caller applies the matching close.
pub(super) async fn pump_tcp_duplex<S>(
    local: S,
    sender: OnionStreamSender,
    receiver: OnionStreamReceiver,
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
        RELAY_IDLE_TIMEOUT,
    )
    .await;
    (read.unsplit(write), OnionPumpEnd::of(state))
}

/// The select loop of [`pump_tcp_duplex`]: returns when both halves close, on the first
/// failure, or after `idle_timeout` without traffic, leaving the reached halves in `state`.
async fn pump_halves<S>(
    read: &mut ReadHalf<S>,
    write: &mut WriteHalf<S>,
    state: &mut TcpDuplexState,
    mut sender: OnionStreamSender,
    mut receiver: OnionStreamReceiver,
    idle_timeout: std::time::Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; TCP_BUF];
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    while !state.is_closed() {
        tokio::select! {
            read_result = read.read(buffer.as_mut_slice()), if state.can_read() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
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
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
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
            () = &mut idle => return,
        }
    }
}
