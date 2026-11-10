//! Bounded async byte bridge between one reconstructed TCP endpoint and one Onion stream.

use std::pin::Pin;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::BoxOnionStreamProbe;
use crate::FlowId;
use crate::GatewayError;
use crate::OnionStreamConnector;

const FLOW_COMMAND_CAPACITY: usize = 1;

#[derive(Debug)]
pub(super) enum BridgeCommand {
    Data(Vec<u8>),
    CloseWrite,
}

pub(super) enum BridgeEvent {
    /// The stream opened; its probe names every later end of the flow (`crate::reset`).
    Opened {
        flow: FlowId,
        probe: BoxOnionStreamProbe,
    },
    Data {
        flow: FlowId,
        bytes: Vec<u8>,
        consumed: oneshot::Sender<()>,
    },
    ClientBuffer {
        flow: FlowId,
        buffer: Vec<u8>,
    },
    PeerClosed(FlowId),
    Failed {
        flow: FlowId,
        error: GatewayError,
    },
}

pub(super) struct BridgeHandle {
    commands: mpsc::Sender<BridgeCommand>,
    task: tokio::task::JoinHandle<()>,
}

impl BridgeHandle {
    pub(super) fn try_send(
        &self,
        command: BridgeCommand,
    ) -> Result<(), mpsc::error::TrySendError<BridgeCommand>> {
        self.commands.try_send(command)
    }

    pub(super) fn abort(self) {
        self.task.abort();
    }
}

impl Drop for BridgeHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn spawn_bridge(
    flow: FlowId,
    connector: Arc<dyn OnionStreamConnector>,
    events: mpsc::Sender<BridgeEvent>,
    stream_buffer_bytes: usize,
    io_chunk_bytes: usize,
) -> BridgeHandle {
    let (gateway, onion) = tokio::io::duplex(stream_buffer_bytes);
    let (commands, command_rx) = mpsc::channel(FLOW_COMMAND_CAPACITY);
    let task = tokio::spawn(run_bridge(
        flow,
        connector,
        events,
        gateway,
        onion,
        command_rx,
        io_chunk_bytes,
    ));
    BridgeHandle { commands, task }
}

enum PendingUpload {
    Data { bytes: Vec<u8>, written: usize },
    Shutdown,
}

enum UploadPoll {
    Wrote(usize),
    ShutDown,
}

async fn poll_pending_consumption(
    pending: &mut Option<oneshot::Receiver<()>>,
) -> Result<(), oneshot::error::RecvError> {
    match pending.as_mut() {
        Some(wait) => wait.await,
        None => std::future::pending().await,
    }
}

async fn poll_pending_upload(
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    pending: Option<&PendingUpload>,
) -> std::io::Result<UploadPoll> {
    match pending {
        Some(PendingUpload::Data { bytes, written }) => {
            let remaining = bytes.get(*written..).unwrap_or_default();
            std::future::poll_fn(|cx| Pin::new(&mut *writer).poll_write(cx, remaining))
                .await
                .map(UploadPoll::Wrote)
        }
        Some(PendingUpload::Shutdown) => {
            std::future::poll_fn(|cx| Pin::new(&mut *writer).poll_shutdown(cx))
                .await
                .map(|()| UploadPoll::ShutDown)
        }
        None => std::future::pending().await,
    }
}

async fn run_bridge(
    flow: FlowId,
    connector: Arc<dyn OnionStreamConnector>,
    events: mpsc::Sender<BridgeEvent>,
    gateway: tokio::io::DuplexStream,
    onion: tokio::io::DuplexStream,
    mut commands: mpsc::Receiver<BridgeCommand>,
    io_chunk_bytes: usize,
) {
    let probe = match connector.open_stream(flow, Box::new(onion)).await {
        Ok(probe) => probe,
        Err(error) => {
            send_event(&events, BridgeEvent::Failed { flow, error }).await;
            return;
        }
    };
    if events
        .send(BridgeEvent::Opened { flow, probe })
        .await
        .is_err()
    {
        return;
    }

    let (mut reader, mut writer) = tokio::io::split(gateway);
    let mut read_open = true;
    let mut write_open = true;
    let mut buffer = vec![0_u8; io_chunk_bytes];
    let mut pending_consumption: Option<oneshot::Receiver<()>> = None;
    let mut pending_upload: Option<PendingUpload> = None;
    // An EOF is forwarded as read; the runtime tells an abortive one apart by the probe.
    while read_open || write_open {
        tokio::select! {
            consumed = poll_pending_consumption(&mut pending_consumption), if pending_consumption.is_some() => {
                pending_consumption = None;
                if consumed.is_err() {
                    return;
                }
            }
            read = reader.read(buffer.as_mut_slice()), if read_open && pending_consumption.is_none() => {
                match read {
                    Ok(0) => {
                        read_open = false;
                        if events.send(BridgeEvent::PeerClosed(flow)).await.is_err() {
                            return;
                        }
                    }
                    Ok(length) => {
                        let Some(bytes) = buffer.get(..length).map(<[u8]>::to_vec) else {
                            send_stream_error(&events, flow, "read", "reader exceeded its buffer").await;
                            return;
                        };
                        let (consumed, wait_for_consumption) = oneshot::channel();
                        if events
                            .send(BridgeEvent::Data {
                                flow,
                                bytes,
                                consumed,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                        pending_consumption = Some(wait_for_consumption);
                    }
                    Err(error) => {
                        send_stream_error(&events, flow, "read", &error.to_string()).await;
                        return;
                    }
                }
            }
            upload = poll_pending_upload(&mut writer, pending_upload.as_ref()), if pending_upload.is_some() => {
                match upload {
                    Ok(UploadPoll::Wrote(0)) => {
                        send_stream_error(&events, flow, "write", "write returned zero bytes").await;
                        return;
                    }
                    Ok(UploadPoll::Wrote(count)) => {
                        let finished = match pending_upload.as_mut() {
                            Some(PendingUpload::Data { bytes, written }) => {
                                *written = written.saturating_add(count);
                                *written >= bytes.len()
                            }
                            _ => false,
                        };
                        if finished {
                            if let Some(PendingUpload::Data { mut bytes, .. }) = pending_upload.take() {
                                bytes.clear();
                                if events
                                    .send(BridgeEvent::ClientBuffer { flow, buffer: bytes })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                    }
                    Ok(UploadPoll::ShutDown) => {
                        pending_upload = None;
                        write_open = false;
                    }
                    Err(error) => {
                        let operation = match pending_upload {
                            Some(PendingUpload::Shutdown) => "shutdown",
                            _ => "write",
                        };
                        send_stream_error(&events, flow, operation, &error.to_string()).await;
                        return;
                    }
                }
            }
            command = commands.recv(), if pending_upload.is_none() => {
                match command {
                    Some(BridgeCommand::Data(bytes)) if write_open => {
                        if bytes.is_empty() {
                            if events
                                .send(BridgeEvent::ClientBuffer { flow, buffer: bytes })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        } else {
                            pending_upload = Some(PendingUpload::Data { bytes, written: 0 });
                        }
                    }
                    Some(BridgeCommand::Data(_)) => {}
                    Some(BridgeCommand::CloseWrite) if write_open => {
                        pending_upload = Some(PendingUpload::Shutdown);
                    }
                    Some(BridgeCommand::CloseWrite) => {
                        write_open = false;
                    }
                    None => {
                        if write_open {
                            let _ = writer.shutdown().await;
                        }
                        return;
                    }
                }
            }
        }
    }
}

async fn send_stream_error(
    events: &mpsc::Sender<BridgeEvent>,
    flow: FlowId,
    operation: &'static str,
    message: &str,
) {
    send_event(events, BridgeEvent::Failed {
        flow,
        error: GatewayError::StreamIo {
            flow,
            operation,
            message: message.to_string(),
        },
    })
    .await;
}

async fn send_event(events: &mpsc::Sender<BridgeEvent>, event: BridgeEvent) {
    let _ = events.send(event).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::unobserved_stream;
    use crate::BoxGatewayDuplex;
    use crate::BoxOnionStreamProbe;

    struct EchoConnector;

    struct BurstConnector;

    #[async_trait::async_trait]
    impl OnionStreamConnector for EchoConnector {
        async fn open_stream(
            &self,
            _flow: FlowId,
            mut stream: BoxGatewayDuplex,
        ) -> Result<BoxOnionStreamProbe, GatewayError> {
            tokio::spawn(async move {
                let mut buffer = [0_u8; 32];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => {
                            let _ = stream.shutdown().await;
                            break;
                        }
                        Ok(length) => {
                            let Some(bytes) = buffer.get(..length) else {
                                break;
                            };
                            if stream.write_all(bytes).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            Ok(unobserved_stream())
        }
    }

    #[async_trait::async_trait]
    impl OnionStreamConnector for BurstConnector {
        async fn open_stream(
            &self,
            _flow: FlowId,
            mut stream: BoxGatewayDuplex,
        ) -> Result<BoxOnionStreamProbe, GatewayError> {
            tokio::spawn(async move {
                let _ = stream.write_all(b"abcdefgh").await;
            });
            Ok(unobserved_stream())
        }
    }

    fn flow() -> FlowId {
        FlowId {
            source: "100.64.0.2:41000".parse().expect("test source"),
            target: "93.184.216.34:443".parse().expect("test target"),
        }
    }

    #[tokio::test]
    async fn bridge_preserves_data_and_half_close() {
        let (events_tx, mut events_rx) = mpsc::channel(4);
        let handle = spawn_bridge(flow(), Arc::new(EchoConnector), events_tx, 64, 32);
        let opened = events_rx.recv().await;
        assert!(matches!(opened, Some(BridgeEvent::Opened { flow: id, .. }) if id == flow()));
        handle
            .try_send(BridgeCommand::Data(b"echo".to_vec()))
            .expect("empty bounded command queue");
        let echoed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match events_rx.recv().await {
                    Some(BridgeEvent::ClientBuffer { .. }) => {}
                    event => return event,
                }
            }
        })
        .await
        .expect("echo deadline");
        let Some(BridgeEvent::Data {
            flow: id,
            bytes,
            consumed,
        }) = echoed
        else {
            panic!("echo data event was not observed");
        };
        assert_eq!(id, flow());
        assert_eq!(bytes, b"echo");
        consumed.send(()).expect("acknowledge consumed echo");
        handle
            .try_send(BridgeCommand::CloseWrite)
            .expect("empty bounded command queue");
        let closed = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("close deadline");
        assert!(matches!(closed, Some(BridgeEvent::PeerClosed(id)) if id == flow()));
    }

    #[tokio::test]
    async fn bridge_waits_for_tcp_consumption_before_reading_more_onion_data() {
        let (events_tx, mut events_rx) = mpsc::channel(4);
        let handle = spawn_bridge(flow(), Arc::new(BurstConnector), events_tx, 64, 4);
        let opened = events_rx.recv().await;
        assert!(matches!(opened, Some(BridgeEvent::Opened { flow: id, .. }) if id == flow()));

        let first = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("first data deadline");
        let Some(BridgeEvent::Data {
            bytes, consumed, ..
        }) = first
        else {
            panic!("first data event was not observed");
        };
        assert_eq!(bytes, b"abcd");
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events_rx.recv())
                .await
                .is_err(),
            "the bridge read a second chunk before TCP consumed the first"
        );

        consumed.send(()).expect("acknowledge first chunk");
        let second = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("second data deadline");
        let Some(BridgeEvent::Data {
            bytes, consumed, ..
        }) = second
        else {
            panic!("second data event was not observed");
        };
        assert_eq!(bytes, b"efgh");
        consumed.send(()).expect("acknowledge second chunk");
        handle.abort();
    }

    #[tokio::test]
    async fn dropping_handle_aborts_bridge_task() {
        let (events_tx, mut events_rx) = mpsc::channel(1);
        let handle = spawn_bridge(flow(), Arc::new(EchoConnector), events_tx, 64, 32);
        let opened = events_rx.recv().await;
        assert!(matches!(opened, Some(BridgeEvent::Opened { flow: id, .. }) if id == flow()));
        let task = handle.task.abort_handle();
        drop(handle);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
    }

    struct DuplexHarnessConnector {
        stream_tx: mpsc::Sender<BoxGatewayDuplex>,
    }

    #[async_trait::async_trait]
    impl OnionStreamConnector for DuplexHarnessConnector {
        async fn open_stream(
            &self,
            _flow: FlowId,
            stream: BoxGatewayDuplex,
        ) -> Result<BoxOnionStreamProbe, GatewayError> {
            self.stream_tx
                .send(stream)
                .await
                .expect("deliver stream to test harness");
            Ok(unobserved_stream())
        }
    }

    #[tokio::test]
    async fn download_backpressure_does_not_block_upload_progress_or_close_write() {
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let (stream_tx, mut stream_rx) = mpsc::channel(1);
        let handle = spawn_bridge(
            flow(),
            Arc::new(DuplexHarnessConnector { stream_tx }),
            events_tx,
            64,
            4,
        );
        let mut upstream = stream_rx.recv().await.expect("upstream stream");
        let opened = events_rx.recv().await;
        assert!(matches!(opened, Some(BridgeEvent::Opened { flow: id, .. }) if id == flow()));

        upstream
            .write_all(b"abcdEFGH")
            .await
            .expect("write two download chunks");
        upstream.shutdown().await.expect("half-close download");

        let first = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("first download chunk deadline");
        let Some(BridgeEvent::Data {
            bytes: first_bytes,
            consumed: first_consumed,
            ..
        }) = first
        else {
            panic!("expected first download chunk");
        };
        assert_eq!(first_bytes, b"abcd");

        // Hold `first_consumed` unacknowledged to simulate TCP download backpressure, and verify
        // that client->upstream uploads and CloseWrite still progress independently.
        handle
            .try_send(BridgeCommand::Data(b"up-1".to_vec()))
            .expect("queue upload while download ack is held");
        let mut up_buf = [0_u8; 4];
        tokio::time::timeout(Duration::from_secs(1), upstream.read_exact(&mut up_buf))
            .await
            .expect("upload progressed while download ack was held")
            .expect("read uploaded bytes");
        assert_eq!(&up_buf, b"up-1");

        let returned = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("client buffer return deadline");
        assert!(
            matches!(returned, Some(BridgeEvent::ClientBuffer { flow: id, .. }) if id == flow())
        );

        handle
            .try_send(BridgeCommand::CloseWrite)
            .expect("queue CloseWrite while download ack is held");
        let mut tail = Vec::new();
        let eof = tokio::time::timeout(Duration::from_secs(1), upstream.read_to_end(&mut tail))
            .await
            .expect("CloseWrite progressed while download ack was held")
            .expect("read upstream EOF");
        assert_eq!(eof, 0);

        // Releasing the download acknowledgement resumes the paused download half.
        first_consumed
            .send(())
            .expect("release first download acknowledgement");
        let second = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("second download chunk deadline");
        let Some(BridgeEvent::Data {
            bytes: second_bytes,
            consumed: second_consumed,
            ..
        }) = second
        else {
            panic!("expected second download chunk");
        };
        assert_eq!(second_bytes, b"EFGH");
        second_consumed
            .send(())
            .expect("release second download acknowledgement");

        let closed = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("peer closed deadline");
        assert!(matches!(closed, Some(BridgeEvent::PeerClosed(id)) if id == flow()));
    }

    #[tokio::test]
    async fn upload_backpressure_does_not_block_download_progress_or_peer_closed() {
        let (events_tx, mut events_rx) = mpsc::channel(8);
        let (stream_tx, mut stream_rx) = mpsc::channel(1);
        let handle = spawn_bridge(
            flow(),
            Arc::new(DuplexHarnessConnector { stream_tx }),
            events_tx,
            8,
            4,
        );
        let mut upstream = stream_rx.recv().await.expect("upstream stream");
        let opened = events_rx.recv().await;
        assert!(matches!(opened, Some(BridgeEvent::Opened { flow: id, .. }) if id == flow()));

        // Send a 32-byte upload into an 8-byte DuplexStream without reading `upstream` yet,
        // forcing the upload write into a backpressured partial-write state.
        let upload_payload = b"0123456789abcdef0123456789abcdef".to_vec();
        handle
            .try_send(BridgeCommand::Data(upload_payload.clone()))
            .expect("queue large upload");
        tokio::task::yield_now().await;

        // While upload is blocked, upstream writes download data and closes its write half.
        upstream
            .write_all(b"down")
            .await
            .expect("write download while upload blocked");
        upstream
            .shutdown()
            .await
            .expect("close upstream write while upload blocked");

        let down = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("download chunk progressed while upload blocked");
        let Some(BridgeEvent::Data {
            bytes, consumed, ..
        }) = down
        else {
            panic!("expected download chunk while upload blocked");
        };
        assert_eq!(bytes, b"down");
        consumed.send(()).expect("acknowledge download chunk");

        let peer_closed = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("PeerClosed progressed while upload blocked");
        assert!(matches!(peer_closed, Some(BridgeEvent::PeerClosed(id)) if id == flow()));

        // Draining `upstream` unblocks the remaining upload bytes and returns `ClientBuffer`.
        let mut received_upload = vec![0_u8; upload_payload.len()];
        tokio::time::timeout(
            Duration::from_secs(1),
            upstream.read_exact(&mut received_upload),
        )
        .await
        .expect("drain blocked upload")
        .expect("read exact upload");
        assert_eq!(received_upload, upload_payload);

        let client_buf = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
            .await
            .expect("ClientBuffer returned after upload drained");
        assert!(
            matches!(client_buf, Some(BridgeEvent::ClientBuffer { flow: id, .. }) if id == flow())
        );
        handle.abort();
    }
}
