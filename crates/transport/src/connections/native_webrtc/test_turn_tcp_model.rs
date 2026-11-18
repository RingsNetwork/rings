//! Stateright model check of the native TURN-over-TCP bridge transition algebra
//! ([`turn_bridge_outbound_decision`] and [`turn_bridge_inbound_decision`]), with mutant
//! refutation for every TURN bridge defect.
//!
//! ```text
//!   BridgeState = TurnBridgePhase (Active | Quiescing | Cancelled)
//!               × 𝔹 (srflx Binding Request probe pending)
//!               × 𝔹 (bogus TCP-backed srflx candidate or spurious TCP session opened)
//!               × 𝔹 (srflx probe stalled without response)
//!               × 𝔹 (TURN Refresh(0) teardown request in flight)
//!               × 𝔹 (TURN Refresh(0) teardown completed)
//!               × 𝔹 (relayed peer data leaked to UDP socket during Quiescing/Cancelled)
//! ```
//!
//! Properties checked across all reachable interleavings:
//!
//! - `Always` no bogus `srflx` or 5 s stall: a bare STUN `Binding Request` (`0x0001`) sent to the
//!   local UDP bridge is never forwarded over TCP and never dropped silently; it is immediately
//!   rejected locally with a `400 Bad Request` response (`!bogus_tcp_srflx && !srflx_stalled`).
//! - `Always` quiescence data isolation: once in `Quiescing` or `Cancelled`, inbound relayed peer
//!   data (`ChannelData` / `Data Indication`) is never delivered to the local UDP socket
//!   (`!relayed_data_leaked_during_quiesce`).
//! - `Always` teardown control liveness: while in `Quiescing`, inbound STUN control responses
//!   (`Refresh(0)` response) are never dropped by the bridge (`!control_response_dropped_in_quiesce`).

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
    bogus_tcp_srflx: bool,
    srflx_stalled: bool,
    relayed_data_leaked_during_quiesce: bool,
    control_response_dropped_in_quiesce: bool,
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
    /// `WebrtcConnection::close()` calls `quiesce_inbound()` and `Client::close()` sends
    /// `Refresh(0)`.
    BeginQuiesceAndSendRefreshZero,
    /// The TURN server's `Refresh(0)` response arrives on the TCP stream.
    InboundRefreshZeroResponse,
    /// `WebrtcConnection::close()` finishes and calls `guard.cancel()`.
    CancelBridge,
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
        match (self.mutant, class) {
            (
                Some(BridgeMutant::ForwardBindingRequestOverTcp),
                OutboundTurnClass::StunBindingRequest(tx),
            ) if phase != TurnBridgePhase::Cancelled => {
                OutboundTurnDecision::ForwardControlRequest(tx)
            }
            (
                Some(BridgeMutant::DropBindingRequestSilently),
                OutboundTurnClass::StunBindingRequest(_),
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
            if state.refresh_zero_in_flight && !state.teardown_completed {
                actions.push(BridgeAction::InboundRefreshZeroResponse);
            }
            actions.push(BridgeAction::CancelBridge);
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
            BridgeAction::BeginQuiesceAndSendRefreshZero => {
                next.phase = TurnBridgePhase::Quiescing;
                let out = self
                    .outbound_decision(next.phase, OutboundTurnClass::TurnControlRequest(DUMMY_TX));
                if matches!(out, OutboundTurnDecision::ForwardControlRequest(_)) {
                    next.refresh_zero_in_flight = true;
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
            BridgeAction::CancelBridge => {
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
