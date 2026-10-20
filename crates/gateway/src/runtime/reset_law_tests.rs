//! The reset law on the runtime (#950): every path of the `crate::reset` model that ends a flow,
//! driven deterministically on the runtime's own clock.

use super::stream_reset_tests::SetProbe;
use super::*;
use crate::OnionStreamCondition;

/// A connector whose open never completes: the flow stays `Opening` until the test injects the
/// bridge's events itself.
struct PendingConnector;

#[async_trait::async_trait]
impl OnionStreamConnector for PendingConnector {
    async fn open_stream(
        &self,
        _flow: FlowId,
        _stream: BoxGatewayDuplex,
    ) -> Result<BoxOnionStreamProbe, GatewayError> {
        std::future::pending().await
    }
}

/// The flow idle timeout `T` of these tests.
const IDLE: Duration = Duration::from_secs(1);

/// An instant past the client's last segment (at 1 ms) by more than `T`.
const PAST_IDLE: Duration = Duration::from_secs(2);

/// A runtime with `T = IDLE` holding one flow whose handshake completed at 1 ms, so its bridge
/// awaits the connector: the flow is `Opening`.
fn opening_flow() -> (GatewayRuntime, FlowId) {
    let mut candidate = config();
    candidate.flow_idle_timeout = IDLE;
    let mut runtime =
        GatewayRuntime::new(candidate, Arc::new(PendingConnector), 53).expect("valid runtime");
    runtime
        .activate("memory-tun".to_string())
        .expect("activate runtime");
    let flow = match runtime
        .ingest_packet(
            client_packet(TcpControl::Syn, TcpSeqNumber(7), None),
            Duration::ZERO,
        )
        .expect("capture flow")
    {
        PacketOutcome::Consumed(flow) => flow,
        _ => panic!("SYN was not consumed"),
    };
    let syn_ack = runtime
        .tcp
        .take_egress()
        .into_iter()
        .next()
        .expect("SYN-ACK");
    runtime
        .ingest_packet(handshake_ack(&syn_ack), Duration::from_millis(1))
        .expect("complete handshake");
    runtime
        .reconcile_flow(flow, Duration::from_millis(1))
        .expect("bridge starts on an established flow");
    assert_eq!(
        runtime.server.flow_state(flow),
        Some(FlowState::Opening(flow))
    );
    (runtime, flow)
}

/// [`opening_flow`] whose stream opened with `probe`: the flow is `Established`.
fn open_flow(probe: &SetProbe) -> (GatewayRuntime, FlowId) {
    let (mut runtime, flow) = opening_flow();
    runtime
        .handle_bridge_event(
            BridgeEvent::Opened {
                flow,
                probe: probe.boxed(),
            },
            Duration::from_millis(2),
        )
        .expect("stream opens");
    assert_eq!(
        runtime.server.flow_state(flow),
        Some(FlowState::Established(flow))
    );
    (runtime, flow)
}

/// Run the runtime's clock to `PAST_IDLE`: smoltcp's idle clock closes the endpoint, and the
/// reconciliation names the reset. Returns the reason recorded.
fn run_idle_clock(runtime: &mut GatewayRuntime) -> Option<FlowResetReason> {
    runtime
        .process_input(LoopInput::Tick, &[], PAST_IDLE)
        .expect("the clock is processed");
    runtime
        .reconcile_all(PAST_IDLE)
        .expect("the closed flow is released");
    let status = runtime.status_handle().snapshot();
    assert_eq!(status.active_flows, 0);
    status.last_flow_reset
}

/// Whether any packet the runtime emitted carries `fin`.
fn emitted_fin(runtime: &mut GatewayRuntime) -> bool {
    runtime
        .tcp
        .take_egress()
        .iter()
        .any(|packet| TcpObservation::parse(packet).fin)
}

/// Law Idle, ⇐: the client silent for `T` while the open stream is live is an idle timeout.
#[tokio::test]
async fn test_idle_clock_on_a_live_stream_is_an_idle_timeout() {
    let (mut runtime, _) = open_flow(&SetProbe::default());
    assert_eq!(
        run_idle_clock(&mut runtime),
        Some(FlowResetReason::IdleTimeout)
    );
}

/// Law Phase: the clock expiring while the open is in progress names the open, not idleness.
#[tokio::test]
async fn test_idle_clock_while_opening_is_an_open_expiry() {
    let (mut runtime, _) = opening_flow();
    assert_eq!(
        run_idle_clock(&mut runtime),
        Some(FlowResetReason::OpenExpired)
    );
}

/// Law Phase: the clock expiring while a frame of the stream is known missing names the stall.
#[tokio::test]
async fn test_idle_clock_on_a_stalled_stream_names_the_stall() {
    let probe = SetProbe::default();
    let (mut runtime, _) = open_flow(&probe);
    probe.set(OnionStreamCondition::Stalled);
    assert_eq!(
        run_idle_clock(&mut runtime),
        Some(FlowResetReason::StreamStalled)
    );
}

/// Law Dominance: a failure the probe reads when the clock expires names the reset, though its
/// bridge event has not reached the runtime.
#[tokio::test]
async fn test_failure_queued_behind_the_idle_clock_names_the_reset() {
    let probe = SetProbe::default();
    let (mut runtime, _) = open_flow(&probe);
    probe.set(OnionStreamCondition::Failed("gap".to_string()));
    assert_eq!(
        run_idle_clock(&mut runtime),
        Some(FlowResetReason::StreamReset("gap".to_string()))
    );
}

/// Law Dominance on `eof`: an EOF read while the probe reads `Failed` resets the flow and never
/// half-closes it with a FIN (#902).
#[tokio::test]
async fn test_eof_of_a_failed_stream_resets_and_never_half_closes() {
    let probe = SetProbe::default();
    let (mut runtime, flow) = open_flow(&probe);
    let _ = runtime.tcp.take_egress();
    probe.set(OnionStreamCondition::Failed("abort".to_string()));
    runtime
        .handle_bridge_event(BridgeEvent::PeerClosed(flow), Duration::from_millis(3))
        .expect("the EOF is classified");
    let status = runtime.status_handle().snapshot();
    assert_eq!(status.active_flows, 0);
    assert_eq!(
        status.last_flow_reset,
        Some(FlowResetReason::StreamReset("abort".to_string()))
    );
    assert!(!emitted_fin(&mut runtime));
}

/// Law Dominance on `bridge`: a write the ended duplex refuses after a failure names the failure,
/// not the refused write.
#[tokio::test]
async fn test_bridge_error_after_a_failure_names_the_failure() {
    let probe = SetProbe::default();
    let (mut runtime, flow) = open_flow(&probe);
    probe.set(OnionStreamCondition::Failed("link retired".to_string()));
    runtime
        .handle_bridge_event(
            BridgeEvent::Failed {
                flow,
                error: GatewayError::StreamIo {
                    flow,
                    operation: "write",
                    message: "broken pipe".to_string(),
                },
            },
            Duration::from_millis(3),
        )
        .expect("the bridge failure is classified");
    assert_eq!(
        runtime.status_handle().snapshot().last_flow_reset,
        Some(FlowResetReason::StreamReset("link retired".to_string()))
    );
}

/// An EOF of a live stream is an orderly end: it half-closes the flow with a FIN.
#[tokio::test]
async fn test_eof_of_a_live_stream_half_closes() {
    let (mut runtime, flow) = open_flow(&SetProbe::default());
    runtime
        .handle_bridge_event(BridgeEvent::PeerClosed(flow), Duration::from_millis(3))
        .expect("the EOF is classified");
    runtime
        .reconcile_flow(flow, Duration::from_millis(3))
        .expect("the EOF half-closes the client direction");
    assert_eq!(
        runtime.server.flow_state(flow),
        Some(FlowState::HalfClosed(flow))
    );
    assert!(emitted_fin(&mut runtime));
}
