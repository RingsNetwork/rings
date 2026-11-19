//! Stateright model check of the native TURN-over-TCP bridge transition algebra
//! ([`turn_bridge_outbound_decision`] and [`turn_bridge_inbound_decision`]), with mutant
//! refutation for every TURN bridge defect.
//!
//! ```text
//!   BridgeState = TurnBridgePhase (Active | Quiescing | Draining | Cancelled)
//!               × 𝔹 (srflx Binding Request probe pending)
//!               × 𝔹 (bogus TCP-backed srflx candidate or spurious TCP session opened)
//!               × 𝔹 (srflx probe stalled without response)
//!               × 𝔹 (TURN Refresh(0) teardown request in flight)
//!               × 𝔹 (TURN Refresh(0) teardown completed)
//!               × ℕ (queued outbound teardown frames awaiting TCP stream write)
//!               × 𝔹 (queued outbound teardown frame lost by cancelling before drain)
//!               × 𝔹 (relayed peer data leaked to UDP socket during Quiescing/Draining/Cancelled)
//! ```
//!
//! Properties checked across all reachable interleavings:
//!
//! - `Always` no bogus `srflx` or 5 s stall: a bare STUN `Binding Request` (`0x0001`) sent to the
//!   local UDP bridge is never forwarded over TCP and never dropped silently; it is immediately
//!   rejected locally with a `400 Bad Request` response (`!bogus_tcp_srflx && !srflx_stalled`).
//! - `Always` quiescence data isolation: once in `Quiescing`, `Draining`, or `Cancelled`, inbound
//!   relayed peer data (`ChannelData` / `Data Indication`) is never delivered to the local UDP
//!   socket (`!relayed_data_leaked_during_quiesce`).
//! - `Always` teardown control liveness: while in `Quiescing`, inbound STUN control responses
//!   (`Refresh(0)` response) are never dropped by the bridge (`!control_response_dropped_in_quiesce`).
//! - `Always` outbound teardown drain: `shutdown()` transitions `Quiescing -> Draining` and flushes
//!   all queued outbound teardown frames (`DTLS close_notify` and `TURN Refresh(0)`) to TCP before
//!   entering `Cancelled` (`!queued_teardown_lost_on_cancel`).

use stateright::Checker;
use stateright::Model;
use stateright::Property;

use super::turn_tcp::turn_bridge_inbound_decision;
use super::turn_tcp::turn_bridge_outbound_decision;
use super::turn_tcp::InboundTurnDecision;
use super::turn_tcp::OutboundTurnClass;
use super::turn_tcp::OutboundTurnDecision;
use super::turn_tcp::TurnBridgePhase;

const DUMMY_TX: [u8; 12] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
struct BridgeModelState {
    phase: TurnBridgePhase,
    allocated: bool,
    refresh_zero_in_flight: bool,
    teardown_completed: bool,
    outbound_queued_teardown_frames: u8,
    bogus_tcp_srflx: bool,
    srflx_stalled: bool,
    relayed_data_leaked_during_quiesce: bool,
    control_response_dropped_in_quiesce: bool,
    outbound_close_notify_dropped_in_quiesce: bool,
    queued_teardown_lost_on_cancel: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum BridgeAction {
    /// `webrtc-ice`'s `gather_candidates_srflx` sends a bare STUN `Binding Request` (`0x0001`) to
    /// the local UDP bridge socket.
    SendSrflxBindingProbe,
    /// `webrtc::turn::client::Client` sends `Allocate` and receives its response.
    AllocateTurnRelay,
    /// Remote peer sends `ChannelData` over the TCP TURN stream.
    InboundRelayedChannelData,
    /// Local DTLS sends `close_notify` encapsulated as `ChannelData` / `Send Indication` during
    /// `RTCPeerConnection::close()`.
    OutboundCloseNotifyChannelData,
    /// `WebrtcConnection::close()` calls `quiesce_inbound()` and `Client::close()` sends
    /// `Refresh(0)`.
    BeginQuiesceAndSendRefreshZero,
    /// The TURN server's `Refresh(0)` response arrives on the TCP stream.
    InboundRefreshZeroResponse,
    /// `write_fut` drains one queued outbound teardown frame from `outbound_rx` to the TCP stream.
    FlushQueuedOutboundFrame,
    /// `RTCPeerConnection::close()` returns and calls `guard.shutdown().await`, entering
    /// `TurnBridgePhase::Draining` to flush remaining queued outbound frames.
    BeginShutdownDrain,
    /// `guard.shutdown().await` finishes draining and cancels the bridge.
    FinishShutdownCancel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BridgeMutant {
    /// Bug 1 (Codex P2): forward bare STUN `Binding Request` (`0x0001`) over TCP, opening a second
    /// TCP connection and advertising the TCP source port as a bogus UDP `srflx` candidate.
    ForwardBindingRequestOverTcp,
    /// Bug 2: silently drop bare STUN `Binding Request` (`0x0001`), stalling
    /// `gather_candidates_srflx` for `STUN_GATHER_TIMEOUT` (5 s).
    DropBindingRequestSilently,
    /// Bug 3 (Codex P2): `quiesce_inbound` drops ALL inbound packets including STUN control
    /// responses, causing `Client::close()` to hang for 7.8 s waiting for `Refresh(0)`.
    QuiesceDropsControlResponses,
    /// Bug 4: `quiesce_inbound` continues delivering inbound `ChannelData`, starving the TURN
    /// client's internal mutex during teardown under heavy inbound traffic.
    QuiesceForwardsRelayedData,
    /// Bug 5 (Codex P2): `quiesce_inbound` drops outbound `ChannelData` / `Send Indication`
    /// packets, suppressing the DTLS `close_notify` alert sent during `RTCPeerConnection::close()`.
    QuiesceDropsOutboundCloseNotify,
    /// Bug 6 (Codex P2): calling `guard.cancel()` immediately after `RTCPeerConnection::close()`
    /// cancels `write_fut` before queued outbound `DTLS close_notify` or `TURN Refresh(0)` frames
    /// are written to the TCP stream.
    CancelWithoutDrainingOutboundQueue,
}

struct BridgeModel {
    mutant: Option<BridgeMutant>,
}

impl BridgeModel {
    fn outbound_decision(
        &self,
        phase: TurnBridgePhase,
        class: OutboundTurnClass,
    ) -> OutboundTurnDecision {
        match (self.mutant, phase, class) {
            (
                Some(BridgeMutant::ForwardBindingRequestOverTcp),
                TurnBridgePhase::Active | TurnBridgePhase::Quiescing | TurnBridgePhase::Draining,
                OutboundTurnClass::StunBindingRequest(tx),
            ) => OutboundTurnDecision::ForwardControlRequest(tx),
            (
                Some(BridgeMutant::DropBindingRequestSilently),
                _,
                OutboundTurnClass::StunBindingRequest(_),
            ) => OutboundTurnDecision::Drop,
            (
                Some(BridgeMutant::QuiesceDropsOutboundCloseNotify),
                TurnBridgePhase::Quiescing | TurnBridgePhase::Draining,
                OutboundTurnClass::RelayedDataOrIndication,
            ) => OutboundTurnDecision::Drop,
            _ => turn_bridge_outbound_decision(phase, class),
        }
    }

    fn inbound_decision(
        &self,
        phase: TurnBridgePhase,
        is_stun_control_response: bool,
    ) -> InboundTurnDecision {
        match (self.mutant, phase, is_stun_control_response) {
            (
                Some(BridgeMutant::QuiesceDropsControlResponses),
                TurnBridgePhase::Quiescing,
                true,
            ) => InboundTurnDecision::Drop,
            (Some(BridgeMutant::QuiesceForwardsRelayedData), TurnBridgePhase::Quiescing, false) => {
                InboundTurnDecision::DeliverRelayedData
            }
            _ => turn_bridge_inbound_decision(phase, is_stun_control_response),
        }
    }
}

impl Model for BridgeModel {
    type State = BridgeModelState;
    type Action = BridgeAction;

    fn init_states(&self) -> Vec<Self::State> {
        vec![BridgeModelState::default()]
    }

    fn actions(&self, state: &Self::State, actions: &mut Vec<Self::Action>) {
        if state.phase == TurnBridgePhase::Cancelled {
            return;
        }
        if state.phase == TurnBridgePhase::Active {
            actions.push(BridgeAction::SendSrflxBindingProbe);
            if !state.allocated {
                actions.push(BridgeAction::AllocateTurnRelay);
            }
            if state.allocated {
                actions.push(BridgeAction::InboundRelayedChannelData);
                actions.push(BridgeAction::BeginQuiesceAndSendRefreshZero);
            }
        }
        if state.phase == TurnBridgePhase::Quiescing {
            actions.push(BridgeAction::InboundRelayedChannelData);
            if state.outbound_queued_teardown_frames < 2 {
                actions.push(BridgeAction::OutboundCloseNotifyChannelData);
            }
            if state.outbound_queued_teardown_frames > 0 {
                actions.push(BridgeAction::FlushQueuedOutboundFrame);
            }
            if state.refresh_zero_in_flight && !state.teardown_completed {
                actions.push(BridgeAction::InboundRefreshZeroResponse);
            }
            actions.push(BridgeAction::BeginShutdownDrain);
        }
        if state.phase == TurnBridgePhase::Draining {
            actions.push(BridgeAction::InboundRelayedChannelData);
            if state.outbound_queued_teardown_frames > 0 {
                actions.push(BridgeAction::FlushQueuedOutboundFrame);
            } else {
                actions.push(BridgeAction::FinishShutdownCancel);
            }
        }
    }

    fn next_state(&self, last: &Self::State, action: Self::Action) -> Option<Self::State> {
        let mut next = *last;
        match action {
            BridgeAction::SendSrflxBindingProbe => {
                match self
                    .outbound_decision(next.phase, OutboundTurnClass::StunBindingRequest(DUMMY_TX))
                {
                    OutboundTurnDecision::RejectBindingRequestLocally(_) => {}
                    OutboundTurnDecision::ForwardControlRequest(_)
                    | OutboundTurnDecision::ForwardData => {
                        next.bogus_tcp_srflx = true;
                    }
                    OutboundTurnDecision::Drop => {
                        next.srflx_stalled = true;
                    }
                }
            }
            BridgeAction::AllocateTurnRelay => {
                let out = self
                    .outbound_decision(next.phase, OutboundTurnClass::TurnControlRequest(DUMMY_TX));
                let inp = self.inbound_decision(next.phase, true);
                if matches!(out, OutboundTurnDecision::ForwardControlRequest(_))
                    && inp == InboundTurnDecision::DeliverControlResponse
                {
                    next.allocated = true;
                }
            }
            BridgeAction::InboundRelayedChannelData => {
                let inp = self.inbound_decision(next.phase, false);
                if next.phase != TurnBridgePhase::Active
                    && inp == InboundTurnDecision::DeliverRelayedData
                {
                    next.relayed_data_leaked_during_quiesce = true;
                }
            }
            BridgeAction::OutboundCloseNotifyChannelData => {
                let out =
                    self.outbound_decision(next.phase, OutboundTurnClass::RelayedDataOrIndication);
                if out == OutboundTurnDecision::ForwardData {
                    next.outbound_queued_teardown_frames =
                        next.outbound_queued_teardown_frames.saturating_add(1);
                } else if next.phase == TurnBridgePhase::Quiescing {
                    next.outbound_close_notify_dropped_in_quiesce = true;
                }
            }
            BridgeAction::BeginQuiesceAndSendRefreshZero => {
                next.phase = TurnBridgePhase::Quiescing;
                let out = self
                    .outbound_decision(next.phase, OutboundTurnClass::TurnControlRequest(DUMMY_TX));
                if matches!(out, OutboundTurnDecision::ForwardControlRequest(_)) {
                    next.refresh_zero_in_flight = true;
                    next.outbound_queued_teardown_frames =
                        next.outbound_queued_teardown_frames.saturating_add(1);
                }
            }
            BridgeAction::InboundRefreshZeroResponse => {
                match self.inbound_decision(next.phase, true) {
                    InboundTurnDecision::DeliverControlResponse => {
                        next.refresh_zero_in_flight = false;
                        next.teardown_completed = true;
                    }
                    InboundTurnDecision::Drop | InboundTurnDecision::DeliverRelayedData => {
                        next.control_response_dropped_in_quiesce = true;
                    }
                }
            }
            BridgeAction::FlushQueuedOutboundFrame => {
                next.outbound_queued_teardown_frames =
                    next.outbound_queued_teardown_frames.saturating_sub(1);
            }
            BridgeAction::BeginShutdownDrain => {
                if self.mutant == Some(BridgeMutant::CancelWithoutDrainingOutboundQueue) {
                    if next.outbound_queued_teardown_frames > 0 {
                        next.queued_teardown_lost_on_cancel = true;
                    }
                    next.phase = TurnBridgePhase::Cancelled;
                } else {
                    next.phase = TurnBridgePhase::Draining;
                }
            }
            BridgeAction::FinishShutdownCancel => {
                if next.outbound_queued_teardown_frames > 0 {
                    next.queued_teardown_lost_on_cancel = true;
                }
                next.phase = TurnBridgePhase::Cancelled;
            }
        }
        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::<Self>::always(
                "bare STUN Binding Request never opens TCP or produces bogus srflx",
                |_, state| !state.bogus_tcp_srflx,
            ),
            Property::<Self>::always(
                "bare STUN Binding Request is never dropped silently to stall 5s",
                |_, state| !state.srflx_stalled,
            ),
            Property::<Self>::always("quiescing bridge drops inbound relayed data", |_, state| {
                !state.relayed_data_leaked_during_quiesce
            }),
            Property::<Self>::always(
                "quiescing bridge delivers STUN control responses for teardown",
                |_, state| !state.control_response_dropped_in_quiesce,
            ),
            Property::<Self>::always(
                "quiescing bridge forwards outbound DTLS close_notify relayed frame",
                |_, state| !state.outbound_close_notify_dropped_in_quiesce,
            ),
            Property::<Self>::always(
                "shutdown drains all queued outbound teardown frames before cancelling",
                |_, state| !state.queued_teardown_lost_on_cancel,
            ),
        ]
    }
}

#[test]
fn test_turn_tcp_bridge_model_preserves_all_safety_and_teardown_laws() {
    let model = BridgeModel { mutant: None };
    let checker = model.checker().spawn_bfs().join();
    checker.assert_properties();
}

#[test]
fn test_each_turn_tcp_bridge_mutant_is_refuted() {
    let cases = [
        (
            BridgeMutant::ForwardBindingRequestOverTcp,
            "bare STUN Binding Request never opens TCP or produces bogus srflx",
        ),
        (
            BridgeMutant::DropBindingRequestSilently,
            "bare STUN Binding Request is never dropped silently to stall 5s",
        ),
        (
            BridgeMutant::QuiesceDropsControlResponses,
            "quiescing bridge delivers STUN control responses for teardown",
        ),
        (
            BridgeMutant::QuiesceForwardsRelayedData,
            "quiescing bridge drops inbound relayed data",
        ),
        (
            BridgeMutant::QuiesceDropsOutboundCloseNotify,
            "quiescing bridge forwards outbound DTLS close_notify relayed frame",
        ),
        (
            BridgeMutant::CancelWithoutDrainingOutboundQueue,
            "shutdown drains all queued outbound teardown frames before cancelling",
        ),
    ];

    for (mutant, expected_violated_property) in cases {
        let model = BridgeModel {
            mutant: Some(mutant),
        };
        let checker = model.checker().spawn_bfs().join();
        assert!(
            checker.discovery(expected_violated_property).is_some(),
            "TURN bridge mutant {mutant:?} must be refuted by property {expected_violated_property:?}"
        );
    }
}
