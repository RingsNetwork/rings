//! An abortive end of an open Onion stream resets the captured flow with its reason (#902, #963).

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use smoltcp::wire::TcpControl;
use smoltcp::wire::TcpSeqNumber;
use tokio::sync::mpsc;

use super::client_packet;
use super::config;
use super::handshake_ack_from_observation;
use super::receive_matching_tcp;
use super::ChannelPacketIo;
use crate::BoxGatewayDuplex;
use crate::BoxOnionStreamProbe;
use crate::FlowId;
use crate::FlowResetReason;
use crate::GatewayError;
use crate::GatewayRuntime;
use crate::OnionStreamCondition;
use crate::OnionStreamConnector;
use crate::OnionStreamProbe;

/// A probe whose condition the test sets: the circuit as the connector would observe it.
#[derive(Clone, Default)]
pub(super) struct SetProbe(Arc<Mutex<Option<OnionStreamCondition>>>);

impl SetProbe {
    /// Make the probe read `condition` from now on.
    pub(super) fn set(&self, condition: OnionStreamCondition) {
        *self.0.lock().expect("probe slot") = Some(condition);
    }

    /// The probe as the connector hands it to the gateway.
    pub(super) fn boxed(&self) -> BoxOnionStreamProbe {
        Box::new(self.clone())
    }
}

impl OnionStreamProbe for SetProbe {
    /// The condition last set, `Live` before any.
    fn condition(&self) -> OnionStreamCondition {
        self.0
            .lock()
            .expect("probe slot")
            .clone()
            .unwrap_or(OnionStreamCondition::Live)
    }
}

/// A connector that opens every stream with a probe the test sets and keeps the gateway side of
/// the stream, so only the test ends it.
struct ResettableConnector {
    opened: mpsc::Sender<FlowId>,
    probe: SetProbe,
    streams: Arc<Mutex<Vec<BoxGatewayDuplex>>>,
}

#[async_trait::async_trait]
impl OnionStreamConnector for ResettableConnector {
    async fn open_stream(
        &self,
        flow: FlowId,
        stream: BoxGatewayDuplex,
    ) -> Result<BoxOnionStreamProbe, GatewayError> {
        self.streams.lock().expect("stream slot").push(stream);
        self.opened
            .send(flow)
            .await
            .map_err(|error| GatewayError::OnionUnavailable {
                target: flow.target,
                message: error.to_string(),
            })?;
        Ok(self.probe.boxed())
    }
}

/// Law (report before end, #902): a connector that makes its probe read `Failed` and then ends
/// the duplex resets the flow (`RST`) with `StreamReset`: the EOF the drop produces is never
/// forwarded as a FIN, and the reset is never named an orderly close or an idle timeout.
#[tokio::test]
async fn test_an_abortive_stream_end_resets_the_flow_with_its_reason() {
    let (ingress_tx, ingress_rx) = mpsc::channel(8);
    let (egress_tx, mut egress_rx) = mpsc::channel(16);
    let (opened_tx, mut opened_rx) = mpsc::channel(1);
    let probe = SetProbe::default();
    let streams = Arc::new(Mutex::new(Vec::new()));
    let connector = Arc::new(ResettableConnector {
        opened: opened_tx,
        probe: probe.clone(),
        streams: Arc::clone(&streams),
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
    opened_rx.recv().await.expect("stream opened");
    probe.set(OnionStreamCondition::Failed(
        "circuit destroyed".to_string(),
    ));
    streams.lock().expect("stream slot").clear();

    let end = receive_matching_tcp(&mut egress_rx, |packet| packet.rst || packet.fin).await;
    assert!(
        end.rst && !end.fin,
        "a failed stream ended the flow with a FIN"
    );
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
