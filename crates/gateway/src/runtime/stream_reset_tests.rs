//! An abortive end of an open Onion stream resets the captured flow with its reason (#963).

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpSeqNumber;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::client_packet;
use super::config;
use super::handshake_ack_from_observation;
use super::receive_matching_tcp;
use super::ChannelPacketIo;
use crate::BoxGatewayDuplex;
use crate::FlowId;
use crate::FlowResetReason;
use crate::GatewayError;
use crate::GatewayRuntime;
use crate::OnionStreamConnector;
use crate::OnionStreamEnd;

/// The end-report sender of the one stream a [`ResettableConnector`] opened.
type EndSender = oneshot::Sender<Result<(), GatewayError>>;

/// A connector that opens every stream and hands the test the sender of its end report.
struct ResettableConnector {
    opened: mpsc::Sender<FlowId>,
    end: Arc<Mutex<Option<EndSender>>>,
    /// The gateway side of the stream, kept open so only the end report can end it.
    streams: Arc<Mutex<Vec<BoxGatewayDuplex>>>,
}

#[async_trait::async_trait]
impl OnionStreamConnector for ResettableConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<OnionStreamEnd, GatewayError> {
        let (end, reported) = oneshot::channel();
        *self.end.lock().expect("end slot") = Some(end);
        self.streams.lock().expect("stream slot").push(stream);
        self.opened
            .send(flow)
            .await
            .map_err(|error| GatewayError::OnionUnavailable {
                target: flow.target,
                message: error.to_string(),
            })?;
        Ok(reported)
    }
}

/// Law: an open stream whose connector reports an abortive end resets the flow (`RST`) and
/// names the reset `StreamReset`, never an orderly close nor an idle timeout.
#[tokio::test]
async fn test_an_abortive_stream_end_resets_the_flow_with_its_reason() {
    let (ingress_tx, ingress_rx) = mpsc::channel(8);
    let (egress_tx, mut egress_rx) = mpsc::channel(16);
    let (opened_tx, mut opened_rx) = mpsc::channel(1);
    let end = Arc::new(Mutex::new(None));
    let connector = Arc::new(ResettableConnector {
        opened: opened_tx,
        end: Arc::clone(&end),
        streams: Arc::default(),
    });
    let mut runtime = GatewayRuntime::new(config(), connector, 29).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    let status = runtime.status_handle();
    let stop = Arc::new(AtomicBool::new(false));
    let task_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut device = ChannelPacketIo {
            ingress: ingress_rx,
            egress: egress_tx,
        };
        runtime
            .run(&mut device, || task_stop.load(Ordering::Acquire))
            .await
    });

    ingress_tx
        .send(client_packet(TcpControl::Syn, TcpSeqNumber(7), None))
        .await
        .expect("send SYN");
    let syn_ack = receive_matching_tcp(&mut egress_rx, |packet| {
        packet.acknowledgment.is_some() && !packet.rst
    })
    .await;
    ingress_tx
        .send(handshake_ack_from_observation(&syn_ack))
        .await
        .expect("send handshake ACK");
    let flow = opened_rx.recv().await.expect("stream opened");
    let reset = end.lock().expect("end slot").take().expect("end sender");
    reset
        .send(Err(GatewayError::OnionStreamReset {
            target: flow.target,
            message: "circuit destroyed".to_string(),
        }))
        .expect("bridge awaits the end report");

    let rst = receive_matching_tcp(&mut egress_rx, |packet| packet.rst).await;
    assert!(rst.rst);
    assert_eq!(
        status.snapshot().last_flow_reset,
        Some(FlowResetReason::StreamReset(
            "circuit destroyed".to_string()
        ))
    );
    stop.store(true, Ordering::Release);
    task.await
        .expect("runtime task")
        .expect("clean runtime stop");
}
