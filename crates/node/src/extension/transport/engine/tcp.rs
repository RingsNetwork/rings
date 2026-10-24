//! TCP instance of the relay: the listener (client side) and the bidirectional
//! byte-stream relay loop (server side), with true half-close and abrupt-close handling.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rings_core::dht::Did;
use rings_core::message::REORDER_DEADLINE;
use rings_runtime::Spawner;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::inject_accepted;
use super::reorder::StreamFault;
use super::reorder::StreamReorder;
use super::send_frame;
use super::Outbound;
use super::Pending;
use super::RelayTask;
use super::TransportSessions;
use super::TCP_BUF;
use crate::extension::ext::Scope;
use crate::extension::transport::Frame;
use crate::extension::transport::SessionKey;
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
        let Ok(spawner) = Spawner::current() else {
            tracing::error!("transport listen on {local_addr} requires a runtime");
            return;
        };
        let listener = match TcpListener::bind(local_addr).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("transport listen bind {local_addr} failed: {e:?}");
                return;
            }
        };
        spawner.spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let Some(token) = self.stash_pending(Pending::Tcp(stream, None)) else {
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
    relay_tcp_with_timeouts(task, stream, RELAY_IDLE_TIMEOUT, REORDER_DEADLINE).await;
}

/// [`relay_tcp`] with its idle timeout and its gap deadline given: the law of
/// [`Frame::Data`] in both directions, peer segments reaching the socket through a
/// [`StreamReorder`]. A gap's deadline runs from when its missing segment became the one
/// awaited.
async fn relay_tcp_with_timeouts(
    task: RelayTask,
    stream: TcpStream,
    idle_timeout: Duration,
    reorder_deadline: Duration,
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
    let mut sent: u64 = 0;
    let mut inbound = StreamReorder::default();
    let mut gap: Option<u64> = None;
    let mut buf = vec![0u8; TCP_BUF];
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    let gap_deadline = tokio::time::sleep(reorder_deadline);
    tokio::pin!(gap_deadline);
    'relay: while local_read_open || local_write_open {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read = local_read.read(buf.as_mut_slice()), if local_read_open => {
                idle.as_mut().reset(Instant::now() + idle_timeout);
                match read {
                    Ok(0) => {
                        let _ = send_frame(&scope, peer, Frame::Shutdown {
                            session,
                            from_opener,
                            sent,
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
                            seq: sent,
                            bytes,
                        }).await.is_err() {
                            cancel.cancel();
                            break;
                        }
                        sent += 1;
                    }
                    Err(_) => {
                        cancel.cancel();
                        break;
                    }
                }
            }
            outbound = outbound_rx.recv(), if local_write_open => {
                idle.as_mut().reset(Instant::now() + idle_timeout);
                let arrival = match outbound {
                    Some(Outbound::Segment { seq, bytes }) => inbound.data(seq, bytes),
                    Some(Outbound::Shutdown { sent }) => inbound.fin(sent),
                    Some(Outbound::Datagram(_)) | None => break,
                };
                let ready = match arrival {
                    Ok(ready) => ready,
                    Err(fault) => {
                        reset(&key, fault, &cancel);
                        break;
                    }
                };
                for bytes in ready.segments {
                    if local_write.write_all(bytes.as_ref()).await.is_err() {
                        cancel.cancel();
                        break 'relay;
                    }
                }
                if ready.finished {
                    let _ = local_write.shutdown().await;
                    local_write_open = false;
                }
                // The deadline runs from when the missing segment became the one awaited.
                let missing = inbound.gap();
                if missing.is_some() && missing != gap {
                    gap_deadline.as_mut().reset(Instant::now() + reorder_deadline);
                }
                gap = missing;
            }
            _ = &mut gap_deadline, if gap.is_some() => {
                if let Some(next) = gap {
                    reset(&key, StreamFault::Unfilled { next }, &cancel);
                }
                break;
            }
            _ = &mut idle => break,
        }
    }

    // Only both directions finishing is a clean end; any other end aborts the local socket.
    if local_read_open || local_write_open {
        if let Ok(stream) = local_read.reunite(local_write) {
            let _ = stream.set_zero_linger();
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

/// Reset the session `key` for `fault`: its stream cannot continue without a silent gap.
fn reset(key: &SessionKey, fault: StreamFault, cancel: &CancellationToken) {
    tracing::warn!(
        peer = %key.peer,
        session = key.session.0,
        fault = ?fault,
        "relay stream reset: the peer's stream cannot continue without a gap"
    );
    cancel.cancel();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::net::TcpListener;

    use super::*;
    use crate::error::Error;
    use crate::error::Result;
    use crate::extension::transport::engine::relay_task_for_test;
    use crate::extension::transport::engine::reorder::segment;
    use crate::extension::transport::EffectEnqueue;

    #[tokio::test]
    async fn test_idle_native_tcp_relay_releases_its_session() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(io_error)?;
        let address = listener.local_addr().map_err(io_error)?;
        let client = TcpStream::connect(address).await.map_err(io_error)?;
        let (server, _) = listener.accept().await.map_err(io_error)?;
        let (task, sessions, key) = relay_task_for_test("tcp")?;

        tokio::time::timeout(
            Duration::from_secs(1),
            relay_tcp_with_timeouts(
                task,
                server,
                Duration::from_millis(20),
                Duration::from_secs(60),
            ),
        )
        .await
        .map_err(|_| Error::ExtensionError("idle TCP relay was not reclaimed".to_string()))?;
        drop(client);
        assert!(!sessions.is_live(&key));
        Ok(())
    }

    /// A relay task over a connected local socket pair, running: the local client's end, the
    /// task, and the engine with the session's key. The gap deadline is `deadline`.
    async fn relayed(
        deadline: Duration,
    ) -> Result<(
        TcpStream,
        tokio::task::JoinHandle<()>,
        Arc<TransportSessions>,
        SessionKey,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(io_error)?;
        let address = listener.local_addr().map_err(io_error)?;
        let client = TcpStream::connect(address).await.map_err(io_error)?;
        let (server, _) = listener.accept().await.map_err(io_error)?;
        let (task, sessions, key) = relay_task_for_test("tcp")?;
        let relay = tokio::spawn(relay_tcp_with_timeouts(
            task,
            server,
            Duration::from_secs(60),
            deadline,
        ));
        Ok((client, relay, sessions, key))
    }

    /// Wait for the relay task to end.
    async fn joined(relay: tokio::task::JoinHandle<()>) -> Result<()> {
        relay
            .await
            .map_err(|error| Error::ExtensionError(format!("relay task failed: {error}")))
    }

    /// The peer's segments `seqs`, concatenated.
    fn segments(seqs: impl IntoIterator<Item = u64>) -> Vec<u8> {
        seqs.into_iter()
            .flat_map(|seq| segment(seq).to_vec())
            .collect()
    }

    /// Deliver the peer's segments `seqs` in that order, then (if any) its half-close.
    fn deliver(sessions: &TransportSessions, key: &SessionKey, seqs: &[u64], fin: Option<u64>) {
        let segments = seqs.iter().map(|seq| Outbound::Segment {
            seq: *seq,
            bytes: segment(*seq),
        });
        for outbound in segments.chain(fin.map(|sent| Outbound::Shutdown { sent })) {
            assert_eq!(sessions.deliver(key, outbound), EffectEnqueue::Enqueued);
        }
    }

    /// How the local client's stream ended.
    #[derive(Debug, Eq, PartialEq)]
    enum Ending {
        /// A clean end of stream.
        Eof,
        /// An abortive close.
        Reset,
    }

    /// Read the client's end until it ends: what it read, and how it ended.
    async fn read_to_end(client: &mut TcpStream) -> Result<(Vec<u8>, Ending)> {
        let mut read = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
                .await
                .map_err(|_| Error::ExtensionError("the relayed stream never ended".to_string()));
            match n? {
                Ok(0) => return Ok((read, Ending::Eof)),
                Ok(n) => read.extend_from_slice(buf.get(..n).unwrap_or_default()),
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {
                    return Ok((read, Ending::Reset));
                }
                Err(error) => return Err(io_error(error)),
            }
        }
    }

    /// Segments that arrive out of order reach the local socket in order, and the half-close
    /// that counted them ends the stream cleanly once both are written.
    #[tokio::test]
    async fn test_a_reordered_stream_reaches_the_local_socket_in_order() -> Result<()> {
        let (mut client, relay, sessions, key) = relayed(Duration::from_secs(60)).await?;
        deliver(&sessions, &key, &[1, 0], Some(2));
        client.shutdown().await.map_err(io_error)?;
        assert_eq!(
            read_to_end(&mut client).await?,
            (segments([0, 1]), Ending::Eof)
        );
        joined(relay).await?;
        assert!(!sessions.is_live(&key));
        Ok(())
    }

    /// A lost first, middle or last segment never ends the local stream cleanly: the client reads
    /// the prefix before the gap, and then a reset once the gap deadline passes.
    #[tokio::test]
    async fn test_a_lost_segment_resets_the_local_stream_after_its_prefix() -> Result<()> {
        for lost in [0, 1, 2] {
            let (mut client, relay, sessions, key) = relayed(Duration::from_millis(200)).await?;
            let arrived: Vec<u64> = (0..3).filter(|seq| *seq != lost).collect();
            deliver(&sessions, &key, &arrived, Some(3));
            assert_eq!(
                read_to_end(&mut client).await?,
                (segments(0..lost), Ending::Reset),
                "segment {lost} lost"
            );
            joined(relay).await?;
            assert!(!sessions.is_live(&key));
        }
        Ok(())
    }

    /// A segment beyond the reorder window shows the awaited one can no longer arrive: the
    /// stream is reset at once, without waiting for the deadline.
    #[tokio::test]
    async fn test_a_segment_beyond_the_window_resets_the_stream_at_once() -> Result<()> {
        let (mut client, relay, sessions, key) = relayed(Duration::from_secs(60)).await?;
        let beyond = rings_core::message::REORDER_WINDOW;
        deliver(&sessions, &key, &[beyond], None);
        assert_eq!(read_to_end(&mut client).await?, (Vec::new(), Ending::Reset));
        joined(relay).await?;
        Ok(())
    }

    fn io_error(error: std::io::Error) -> Error {
        Error::ExtensionError(format!("TCP relay test IO failed: {error}"))
    }
}
