//! The duplex pump of a client `tcp` session: one local byte stream against one session.
//!
//! ```text
//! loop select, until both halves close or the stream idles for RELAY_IDLE_TIMEOUT:
//!   local read  n > 0 ─▶ session.send(bytes)      local EOF ─▶ session.fin, close read
//!   session Data(w)   ─▶ local write(w)           session Fin ─▶ local shutdown, close write
//!   session Failed | end ─▶ stop
//! ```
//!
//! Law: the halves close independently (TCP half-close); the local stream is read only while its
//! read half is open, and written only while its write half is.

use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use super::duplex::TcpDuplexState;
use super::TCP_BUF;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;
use crate::onion::session::dial::OnionStreamEvent;
use crate::onion::session::dial::OnionStreamReceiver;
use crate::onion::session::dial::OnionStreamSender;

/// Pump `local` against the session halves (see the module diagram).
pub(super) async fn pump_tcp_duplex<S>(
    local: S,
    sender: OnionStreamSender,
    receiver: OnionStreamReceiver,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pump_tcp_duplex_with_idle(local, sender, receiver, RELAY_IDLE_TIMEOUT).await;
}

/// [`pump_tcp_duplex`] with an explicit idle timeout.
async fn pump_tcp_duplex_with_idle<S>(
    local: S,
    mut sender: OnionStreamSender,
    mut receiver: OnionStreamReceiver,
    idle_timeout: std::time::Duration,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut read, mut write) = tokio::io::split(local);
    let mut buffer = vec![0_u8; TCP_BUF];
    let mut state = TcpDuplexState::open();
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
                            let _ = write.shutdown().await;
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
