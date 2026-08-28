//! Bounded async byte bridge between one reconstructed TCP endpoint and one Onion stream.

use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

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
    Opened(FlowId),
    Data {
        flow: FlowId,
        bytes: Vec<u8>,
        consumed: oneshot::Sender<()>,
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

async fn run_bridge(
    flow: FlowId,
    connector: Arc<dyn OnionStreamConnector>,
    events: mpsc::Sender<BridgeEvent>,
    mut gateway: tokio::io::DuplexStream,
    onion: tokio::io::DuplexStream,
    mut commands: mpsc::Receiver<BridgeCommand>,
    io_chunk_bytes: usize,
) {
    if let Err(error) = connector.open_stream(flow, Box::new(onion)).await {
        send_event(&events, BridgeEvent::Failed { flow, error }).await;
        return;
    }
    if events.send(BridgeEvent::Opened(flow)).await.is_err() {
        return;
    }

    let mut read_open = true;
    let mut write_open = true;
    let mut buffer = vec![0_u8; io_chunk_bytes];
    while read_open || write_open {
        tokio::select! {
            read = gateway.read(buffer.as_mut_slice()), if read_open => {
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
                        if wait_for_consumption.await.is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        send_stream_error(&events, flow, "read", &error.to_string()).await;
                        return;
                    }
                }
            }
            command = commands.recv() => {
                match command {
                    Some(BridgeCommand::Data(bytes)) if write_open => {
                        if let Err(error) = gateway.write_all(&bytes).await {
                            send_stream_error(&events, flow, "write", &error.to_string()).await;
                            return;
                        }
                    }
                    Some(BridgeCommand::Data(_)) => {}
                    Some(BridgeCommand::CloseWrite) | None if write_open => {
                        if let Err(error) = gateway.shutdown().await {
                            send_stream_error(&events, flow, "shutdown", &error.to_string()).await;
                            return;
                        }
                        write_open = false;
                    }
                    Some(BridgeCommand::CloseWrite) | None => {
                        write_open = false;
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
    use crate::BoxGatewayDuplex;

    struct EchoConnector;

    struct BurstConnector;

    #[async_trait::async_trait]
    impl OnionStreamConnector for EchoConnector {
        async fn open_stream(
            &self,
            _flow: FlowId,
            mut stream: BoxGatewayDuplex,
        ) -> Result<(), GatewayError> {
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
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl OnionStreamConnector for BurstConnector {
        async fn open_stream(
            &self,
            _flow: FlowId,
            mut stream: BoxGatewayDuplex,
        ) -> Result<(), GatewayError> {
            tokio::spawn(async move {
                let _ = stream.write_all(b"abcdefgh").await;
            });
            Ok(())
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
        assert!(matches!(events_rx.recv().await, Some(BridgeEvent::Opened(id)) if id == flow()));
        handle
            .try_send(BridgeCommand::Data(b"echo".to_vec()))
            .expect("empty bounded command queue");
        let echoed = tokio::time::timeout(Duration::from_secs(1), events_rx.recv())
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
        assert!(matches!(events_rx.recv().await, Some(BridgeEvent::Opened(id)) if id == flow()));

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
}
