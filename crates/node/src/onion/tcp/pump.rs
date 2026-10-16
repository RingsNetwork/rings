//! Shared imperative TCP duplex pump.

use bytes::Bytes;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use super::duplex::TcpDuplexState;
use super::inbound::TcpInbound;
use super::inbound::TcpInboundLane;
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
pub(super) async fn pump_tcp_duplex<S, E>(
    stream: S,
    inbound: TcpInboundLane,
    effects: &mut E,
) -> TcpStreamEnd
where
    S: AsyncRead + AsyncWrite + Unpin,
    E: TcpDuplexEffects,
{
    pump_tcp_duplex_with_idle(stream, inbound, effects, RELAY_IDLE_TIMEOUT).await
}

/// Why a stream ended before both of its halves closed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TcpStreamAbort {
    /// A frame of the stream could not be sent on its circuit.
    SendFailed,
    /// The local socket failed a read.
    ReadFailed,
    /// The local socket failed a write.
    WriteFailed,
    /// Byte accounting refused a chunk.
    AdmissionRefused,
    /// The far side ended the stream, or its inbound lane closed, while a half was open.
    RemoteClosed,
    /// The far side reported a failure.
    RemoteFailed(OnionExitFailure),
    /// The stream's circuit was destroyed (#963).
    CircuitDestroyed,
    /// A cell from the stream's adjacent hop was dropped here (#966): the link is up, but the
    /// far side has not observed the loss.
    CellDropped,
    /// No traffic for the relay idle timeout.
    Idle,
}

impl TcpStreamAbort {
    /// Whether this end caused the abort, so the far endpoint learns of it only if this end
    /// destroys the circuit; a far-side close, failure or destruction is already known there.
    pub(crate) const fn originates_here(&self) -> bool {
        matches!(
            self,
            Self::SendFailed
                | Self::ReadFailed
                | Self::WriteFailed
                | Self::AdmissionRefused
                | Self::CellDropped
                | Self::Idle
        )
    }
}

impl std::fmt::Display for TcpStreamAbort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SendFailed => formatter.write_str("a frame could not be sent on the circuit"),
            Self::ReadFailed => formatter.write_str("the local socket failed a read"),
            Self::WriteFailed => formatter.write_str("the local socket failed a write"),
            Self::AdmissionRefused => formatter.write_str("byte accounting refused a chunk"),
            Self::RemoteClosed => formatter.write_str("the far side closed the stream"),
            Self::RemoteFailed(failure) => write!(formatter, "the far side failed: {failure}"),
            Self::CircuitDestroyed => formatter.write_str("the circuit was destroyed"),
            Self::CellDropped => formatter.write_str("a cell of the circuit was dropped here"),
            Self::Idle => formatter.write_str("the stream was idle too long"),
        }
    }
}

/// How one pumped stream ended.
///
/// Law: `Orderly` iff both halves closed locally before any other exit of the pump; every other
/// exit is `Aborted` with its cause, so an abortive end is never reported as an EOF (#963).
#[derive(Debug)]
pub(crate) enum TcpStreamEnd {
    /// Both halves closed in order.
    Orderly,
    /// The stream ended while a half was open.
    Aborted(TcpStreamAbort),
}

async fn pump_tcp_duplex_with_idle<S, E>(
    stream: S,
    inbound: TcpInboundLane,
    effects: &mut E,
    idle_timeout: std::time::Duration,
) -> TcpStreamEnd
where
    S: AsyncRead + AsyncWrite + Unpin,
    E: TcpDuplexEffects,
{
    let TcpInboundLane {
        frames: mut inbound,
        fate,
    } = inbound;
    let (mut read, mut write) = tokio::io::split(stream);
    let mut read_buf = vec![0_u8; TCP_BUF];
    let mut state = TcpDuplexState::open();
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    let end = loop {
        if state.is_closed() {
            break TcpStreamEnd::Orderly;
        }
        tokio::select! {
            read_result = read.read(read_buf.as_mut_slice()), if state.can_read() => {
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
                match read_result {
                    Ok(0) => {
                        if effects.send(OnionTcpPayload::Shutdown).await.is_err() {
                            break TcpStreamEnd::Aborted(TcpStreamAbort::SendFailed);
                        }
                        state.close_read();
                    }
                    Ok(n) => {
                        let bytes = match read_chunk(&read_buf, n) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                effects.read_failed(&error).await;
                                break TcpStreamEnd::Aborted(TcpStreamAbort::ReadFailed);
                            }
                        };
                        if !effects.admit_bytes(bytes.len()).await {
                            break TcpStreamEnd::Aborted(TcpStreamAbort::AdmissionRefused);
                        }
                        if effects.send(OnionTcpPayload::Data { bytes }).await.is_err() {
                            break TcpStreamEnd::Aborted(TcpStreamAbort::SendFailed);
                        }
                    }
                    Err(error) => {
                        effects.read_failed(&error).await;
                        break TcpStreamEnd::Aborted(TcpStreamAbort::ReadFailed);
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
                        if !effects.admit_bytes(bytes.len()).await {
                            break TcpStreamEnd::Aborted(TcpStreamAbort::AdmissionRefused);
                        }
                        if write.write_all(bytes.as_ref()).await.is_err() {
                            break TcpStreamEnd::Aborted(TcpStreamAbort::WriteFailed);
                        }
                    }
                    Some(TcpInbound::Shutdown) => {
                        if state.can_write() {
                            let _ = write.shutdown().await;
                            state.close_write();
                        }
                    }
                    Some(TcpInbound::Close) => {
                        state.observe_remote_terminal();
                        break TcpStreamEnd::Aborted(TcpStreamAbort::RemoteClosed);
                    }
                    None => {
                        state.observe_remote_terminal();
                        break TcpStreamEnd::Aborted(fate.closed());
                    }
                    Some(TcpInbound::Error(failure)) => {
                        effects.remote_failed(&failure);
                        state.observe_remote_terminal();
                        break TcpStreamEnd::Aborted(TcpStreamAbort::RemoteFailed(failure));
                    }
                }
            }
            _ = &mut idle => break TcpStreamEnd::Aborted(TcpStreamAbort::Idle),
        }
    };
    if state.should_announce_terminal() {
        let _ = effects.send(OnionTcpPayload::Close).await;
    }
    end
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

    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::sync::mpsc;
    use tokio::sync::Notify;

    use super::super::inbound::tcp_inbound_lane;
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
    async fn test_shared_pump_accepts_a_runtime_neutral_duplex_stream() -> Result<()> {
        let (mut client, gateway) = tokio::io::duplex(128);
        let (inbound_tx, inbound_rx) = tcp_inbound_lane(1);
        let recorded = Arc::new(RecordedEffects::default());
        let task_recorded = Arc::clone(&recorded);
        let pump = tokio::spawn(async move {
            let mut route = RecordingRoute {
                effects: task_recorded,
            };
            pump_tcp_duplex(gateway, inbound_rx, &mut route).await;
        });

        client.write_all(b"generic").await.map_err(test_io_error)?;
        client.shutdown().await.map_err(test_io_error)?;
        tokio::time::timeout(Duration::from_secs(1), recorded.wait_for_shutdown())
            .await
            .map_err(|_| {
                Error::ExtensionError("duplex stream did not observe EOF".to_string())
            })??;
        inbound_tx
            .frames
            .send(TcpInbound::Close)
            .await
            .map_err(|error| Error::ExtensionError(error.to_string()))?;
        tokio::time::timeout(Duration::from_secs(1), pump)
            .await
            .map_err(|_| Error::ExtensionError("duplex stream pump did not stop".to_string()))?
            .map_err(|error| Error::ExtensionError(error.to_string()))?;

        let payloads = lock(&recorded.payloads)?;
        assert!(payloads.iter().any(
            |payload| matches!(payload, OnionTcpPayload::Data { bytes } if bytes.as_ref() == b"generic")
        ));
        Ok(())
    }

    /// Law (#963, #966): the cause a teardown records ends the pump aborted with it once the lane
    /// closes, behind the frames already queued, even when the lane is full; while both halves
    /// are open, and no terminal frame is echoed onto the dead circuit.
    #[tokio::test]
    async fn test_a_recorded_fate_aborts_the_pump_behind_a_full_lane() -> Result<()> {
        for cause in [
            TcpStreamAbort::CircuitDestroyed,
            TcpStreamAbort::CellDropped,
        ] {
            let (mut client, server) = connected_pair().await?;
            let (inbound_tx, inbound_rx) = tcp_inbound_lane(1);
            let recorded = Arc::new(RecordedEffects::default());
            let mut route = RecordingRoute {
                effects: Arc::clone(&recorded),
            };
            inbound_tx
                .frames
                .send(TcpInbound::Data(Bytes::from_static(b"queued")))
                .await
                .map_err(|error| Error::ExtensionError(error.to_string()))?;
            assert!(inbound_tx
                .frames
                .try_send(TcpInbound::Shutdown)
                .is_err_and(|error| matches!(error, mpsc::error::TrySendError::Full(_))));
            inbound_tx.fate.end(cause.clone());
            drop(inbound_tx);

            let end = tokio::time::timeout(
                Duration::from_secs(1),
                pump_tcp_duplex(server, inbound_rx, &mut route),
            )
            .await
            .map_err(|_| Error::ExtensionError("TCP pump did not stop".to_string()))?;

            let mut written = [0_u8; 6];
            client
                .read_exact(&mut written)
                .await
                .map_err(test_io_error)?;
            assert_eq!(&written, b"queued");
            assert!(
                matches!(&end, TcpStreamEnd::Aborted(abort) if *abort == cause),
                "{end:?}"
            );
            assert!(lock(&recorded.payloads)?.is_empty());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_shared_pump_preserves_data_half_close_and_terminal_order() -> Result<()> {
        let (mut client, server) = connected_pair().await?;
        let (inbound_tx, inbound_rx) = tcp_inbound_lane(1);
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
            .frames
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
    async fn test_remote_terminal_suppresses_terminal_echo() -> Result<()> {
        let (_client, server) = connected_pair().await?;
        let (inbound_tx, inbound_rx) = tcp_inbound_lane(1);
        inbound_tx
            .frames
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
    async fn test_idle_stream_is_reclaimed_and_announces_close() -> Result<()> {
        let (_client, server) = connected_pair().await?;
        let (_inbound_tx, inbound_rx) = tcp_inbound_lane(1);
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
    fn test_read_chunk_rejects_a_length_beyond_the_buffer() {
        assert!(read_chunk(&[1, 2], 3).is_err());
    }
}
