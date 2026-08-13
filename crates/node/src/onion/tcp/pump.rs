//! Shared imperative TCP duplex pump.

use bytes::Bytes;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::duplex::TcpDuplexState;
use super::inbound::TcpInbound;
use super::OnionTcpPayload;
use super::TCP_BUF;
use crate::error::Result;
use crate::extension::transport::RELAY_IDLE_TIMEOUT;
use crate::onion::OnionExitFailure;

/// Direction-specific effects around the shared TCP duplex state machine.
///
/// The pump owns socket IO and half-close transitions. Implementations own only route encoding,
/// optional byte admission/accounting, and direction-specific diagnostics.
#[async_trait::async_trait]
pub(super) trait TcpDuplexEffects: Send {
    async fn send(&mut self, payload: OnionTcpPayload) -> Result<()>;

    async fn admit_bytes(&mut self, _bytes: usize) -> bool {
        true
    }

    async fn read_failed(&mut self, _error: &std::io::Error) {}

    fn remote_failed(&mut self, _failure: &OnionExitFailure) {}
}

/// Pump one local TCP stream against its onion inbound lane.
///
/// Invariant: socket-read openness and socket-write openness are independent affine capabilities.
/// Preservation: EOF consumes only the read capability, `Shutdown` consumes only the write
/// capability, and a remote terminal consumes both without echoing another terminal frame.
pub(super) async fn pump_tcp_duplex<E>(
    stream: TcpStream,
    inbound: mpsc::Receiver<TcpInbound>,
    effects: &mut E,
) where
    E: TcpDuplexEffects,
{
    pump_tcp_duplex_with_idle(stream, inbound, effects, RELAY_IDLE_TIMEOUT).await;
}

async fn pump_tcp_duplex_with_idle<E>(
    stream: TcpStream,
    mut inbound: mpsc::Receiver<TcpInbound>,
    effects: &mut E,
    idle_timeout: std::time::Duration,
) where
    E: TcpDuplexEffects,
{
    let (mut read, mut write) = stream.into_split();
    let mut read_buf = vec![0_u8; TCP_BUF];
    let mut state = TcpDuplexState::open();
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    loop {
        if state.is_closed() {
            break;
        }
        tokio::select! {
            read_result = read.read(read_buf.as_mut_slice()), if state.can_read() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                match read_result {
                    Ok(0) => {
                        if effects.send(OnionTcpPayload::Shutdown).await.is_err() {
                            break;
                        }
                        state.close_read();
                    }
                    Ok(n) => {
                        let bytes = match read_chunk(&read_buf, n) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                effects.read_failed(&error).await;
                                break;
                            }
                        };
                        if !effects.admit_bytes(bytes.len()).await
                            || effects.send(OnionTcpPayload::Data { bytes }).await.is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        effects.read_failed(&error).await;
                        break;
                    }
                }
            }
            message = inbound.recv() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                match message {
                    Some(TcpInbound::Data(bytes)) => {
                        if !state.can_write() {
                            continue;
                        }
                        if !effects.admit_bytes(bytes.len()).await
                            || write.write_all(bytes.as_ref()).await.is_err()
                        {
                            break;
                        }
                    }
                    Some(TcpInbound::Shutdown) => {
                        if state.can_write() {
                            let _ = write.shutdown().await;
                            state.close_write();
                        }
                    }
                    Some(TcpInbound::Close) | None => {
                        state.observe_remote_terminal();
                        break;
                    }
                    Some(TcpInbound::Error(failure)) => {
                        effects.remote_failed(&failure);
                        state.observe_remote_terminal();
                        break;
                    }
                }
            }
            _ = &mut idle => break,
        }
    }
    if state.should_announce_terminal() {
        let _ = effects.send(OnionTcpPayload::Close).await;
    }
}

fn read_chunk(read_buf: &[u8], n: usize) -> std::io::Result<Bytes> {
    // Pre: Tokio returns a byte count no larger than the buffer passed to `read`.
    // Post: the returned `Bytes` is exactly the observed prefix; a contract violation is an
    // explicit effect failure and can never bypass exit-byte accounting as an empty payload.
    read_buf
        .get(..n)
        .map(Bytes::copy_from_slice)
        .ok_or_else(|| std::io::Error::other("TCP reader returned a length beyond its buffer"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::io::AsyncWriteExt as _;
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    use super::*;
    use crate::error::Error;
    use crate::sync_lock::lock;

    #[derive(Default)]
    struct RecordedEffects {
        payloads: Mutex<Vec<OnionTcpPayload>>,
        changed: Notify,
    }

    struct RecordingRoute {
        effects: Arc<RecordedEffects>,
    }

    #[async_trait::async_trait]
    impl TcpDuplexEffects for RecordingRoute {
        async fn send(&mut self, payload: OnionTcpPayload) -> Result<()> {
            lock(&self.effects.payloads)?.push(payload);
            self.effects.changed.notify_waiters();
            Ok(())
        }
    }

    impl RecordedEffects {
        async fn wait_for_shutdown(&self) -> Result<()> {
            loop {
                let changed = self.changed.notified();
                if lock(&self.payloads)?
                    .iter()
                    .any(|payload| matches!(payload, OnionTcpPayload::Shutdown))
                {
                    return Ok(());
                }
                changed.await;
            }
        }
    }

    fn test_io_error(error: std::io::Error) -> Error {
        Error::ExtensionError(format!("TCP pump test IO failed: {error}"))
    }

    async fn connected_pair() -> Result<(TcpStream, TcpStream)> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(test_io_error)?;
        let address = listener.local_addr().map_err(test_io_error)?;
        let client = TcpStream::connect(address).await.map_err(test_io_error)?;
        let (server, _) = listener.accept().await.map_err(test_io_error)?;
        Ok((client, server))
    }

    #[tokio::test]
    async fn shared_pump_preserves_data_half_close_and_terminal_order() -> Result<()> {
        let (mut client, server) = connected_pair().await?;
        let (inbound_tx, inbound_rx) = mpsc::channel(1);
        let recorded = Arc::new(RecordedEffects::default());
        let task_recorded = Arc::clone(&recorded);
        let pump = tokio::spawn(async move {
            let mut route = RecordingRoute {
                effects: task_recorded,
            };
            pump_tcp_duplex(server, inbound_rx, &mut route).await;
        });

        client.write_all(b"hello").await.map_err(test_io_error)?;
        client.shutdown().await.map_err(test_io_error)?;
        tokio::time::timeout(Duration::from_secs(1), recorded.wait_for_shutdown())
            .await
            .map_err(|_| Error::ExtensionError("TCP pump did not observe EOF".to_string()))??;
        inbound_tx
            .send(TcpInbound::Shutdown)
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))?;
        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .map_err(|_| Error::ExtensionError("TCP pump did not terminate".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))?;

        let payloads = lock(&recorded.payloads)?;
        let data = payloads
            .iter()
            .filter_map(|payload| match payload {
                OnionTcpPayload::Data { bytes } => Some(bytes.as_ref()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(data, b"hello");
        assert!(matches!(payloads.last(), Some(OnionTcpPayload::Close)));
        assert_eq!(
            payloads
                .iter()
                .filter(|payload| matches!(payload, OnionTcpPayload::Shutdown))
                .count(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_terminal_suppresses_terminal_echo() -> Result<()> {
        let (_client, server) = connected_pair().await?;
        let (inbound_tx, inbound_rx) = mpsc::channel(1);
        inbound_tx
            .send(TcpInbound::Close)
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))?;
        let recorded = Arc::new(RecordedEffects::default());
        let task_recorded = Arc::clone(&recorded);
        let pump = tokio::spawn(async move {
            let mut route = RecordingRoute {
                effects: task_recorded,
            };
            pump_tcp_duplex(server, inbound_rx, &mut route).await;
        });

        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .map_err(|_| Error::ExtensionError("TCP pump did not accept remote close".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))?;
        assert!(lock(&recorded.payloads)?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn idle_stream_is_reclaimed_and_announces_close() -> Result<()> {
        let (_client, server) = connected_pair().await?;
        let (_inbound_tx, inbound_rx) = mpsc::channel(1);
        let recorded = Arc::new(RecordedEffects::default());
        let task_recorded = Arc::clone(&recorded);
        let pump = tokio::spawn(async move {
            let mut route = RecordingRoute {
                effects: task_recorded,
            };
            pump_tcp_duplex_with_idle(server, inbound_rx, &mut route, Duration::from_millis(20))
                .await;
        });

        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .map_err(|_| Error::ExtensionError("idle TCP pump was not reclaimed".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))?;
        assert!(matches!(
            lock(&recorded.payloads)?.last(),
            Some(OnionTcpPayload::Close)
        ));
        Ok(())
    }

    #[test]
    fn read_chunk_rejects_a_length_beyond_the_buffer() {
        assert!(read_chunk(&[1, 2], 3).is_err());
    }
}
