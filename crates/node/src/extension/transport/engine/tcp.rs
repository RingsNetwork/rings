//! TCP instance of the relay: the listener (client side) and the bidirectional
//! byte-stream relay loop (server side), with true half-close and abrupt-close handling.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use rings_core::dht::Did;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;

use super::inject_accepted;
use super::send_frame;
use super::Outbound;
use super::Pending;
use super::RelayTask;
use super::TransportSessions;
use super::TCP_BUF;
use crate::extension::ext::Scope;
use crate::extension::transport::platform::spawn_detached;
use crate::extension::transport::Frame;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;

impl TransportSessions {
    /// Bind a TCP listener; per accepted connection, stash the stream and report the accept
    /// to the pure relay (`Accepted`). The core mints the session id and replies with
    /// `OpenAccepted` → [`bind_accepted`](TransportSessions::bind_accepted), which opens the
    /// peer session and starts the relay loop. The listener itself decides nothing.
    pub(super) async fn listen_tcp(
        self: Arc<Self>,
        scope: Scope,
        local_addr: SocketAddr,
        peer: Did,
        service: String,
    ) {
        let listener = match TcpListener::bind(local_addr).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("transport listen bind {local_addr} failed: {e:?}");
                return;
            }
        };
        spawn_detached(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let Some(token) = self.stash_pending(Pending::Tcp(stream)) else {
                            continue;
                        };
                        if inject_accepted(&scope, token, peer, service.clone())
                            .await
                            .is_err()
                        {
                            self.evict_pending(token);
                        }
                    }
                    Err(e) => {
                        tracing::error!("transport accept on {local_addr} failed: {e:?}");
                        break;
                    }
                }
            }
        });
    }
}

/// Bidirectional TCP relay with true half-close and abrupt-close handling.
pub(super) async fn relay_tcp(task: RelayTask, stream: TcpStream) {
    relay_tcp_with_idle(task, stream, RELAY_IDLE_TIMEOUT).await;
}

async fn relay_tcp_with_idle(
    task: RelayTask,
    stream: TcpStream,
    idle_timeout: std::time::Duration,
) {
    let RelayTask {
        sessions,
        scope,
        key,
        mut outbound_rx,
        cancel,
        generation,
    } = task;
    let peer = key.peer;
    let session = key.session;
    let from_opener = super::opened_by_us(&key);
    let (mut local_read, mut local_write) = stream.into_split();
    let mut local_read_open = true;
    let mut local_write_open = true;
    let mut buf = vec![0u8; TCP_BUF];
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    while local_read_open || local_write_open {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read = local_read.read(buf.as_mut_slice()), if local_read_open => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                match read {
                    Ok(0) => {
                        let _ = send_frame(&scope, peer, Frame::Shutdown {
                            session,
                            from_opener,
                        }).await;
                        local_read_open = false;
                    }
                    Ok(n) => {
                        let Some(chunk) = buf.get(..n) else {
                            cancel.cancel();
                            break;
                        };
                        let bytes = Bytes::copy_from_slice(chunk);
                        if send_frame(&scope, peer, Frame::Data {
                            session,
                            from_opener,
                            bytes,
                        }).await.is_err() {
                            cancel.cancel();
                            break;
                        }
                    }
                    Err(_) => {
                        cancel.cancel();
                        break;
                    }
                }
            }
            outbound = outbound_rx.recv(), if local_write_open => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                match outbound {
                    Some(Outbound::Data(bytes)) => {
                        if local_write.write_all(bytes.as_ref()).await.is_err() {
                            cancel.cancel();
                            break;
                        }
                    }
                    Some(Outbound::Shutdown) => {
                        let _ = local_write.shutdown().await;
                        local_write_open = false;
                    }
                    None => break,
                }
            }
            _ = &mut idle => break,
        }
    }

    // Teardown: drop *our* session instance (generation-checked, so we never delete a newer
    // reuse of the key) — which `Untrack`s it from the pure state. Only tell the peer if we
    // were still the current owner; a stale task must not Close the peer's reused session.
    if sessions.close_if_current(&scope, &key, generation).await {
        let _ = send_frame(&scope, peer, Frame::Close {
            session,
            from_opener,
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;
    use crate::error::Error;
    use crate::error::Result;
    use crate::extension::transport::engine::relay_task_for_test;

    #[tokio::test]
    async fn idle_native_tcp_relay_releases_its_session() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(io_error)?;
        let address = listener.local_addr().map_err(io_error)?;
        let client = TcpStream::connect(address).await.map_err(io_error)?;
        let (server, _) = listener.accept().await.map_err(io_error)?;
        let (task, sessions, key) = relay_task_for_test("tcp")?;

        tokio::time::timeout(
            Duration::from_secs(1),
            relay_tcp_with_idle(task, server, Duration::from_millis(20)),
        )
        .await
        .map_err(|_| Error::ExtensionError("idle TCP relay was not reclaimed".to_string()))?;
        drop(client);
        assert!(!sessions.is_live(&key));
        Ok(())
    }

    fn io_error(error: std::io::Error) -> Error {
        Error::ExtensionError(format!("TCP relay test IO failed: {error}"))
    }
}
