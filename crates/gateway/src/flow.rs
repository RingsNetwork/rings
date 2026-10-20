//! Pure lifecycle model for one captured TCP flow.

use std::net::SocketAddr;

use serde::Deserialize;
use serde::Serialize;

use crate::FlowTransitionError;

/// Stable identity of one captured TCP flow.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FlowId {
    /// Original client socket address observed in the packet stream.
    pub source: SocketAddr,
    /// Original immutable destination socket address observed in the packet stream.
    pub target: SocketAddr,
}

/// Events accepted by the flow lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowEvent {
    /// A valid route was selected and the onion stream is opening.
    Open,
    /// The exit confirmed that its public TCP connection is established.
    Establish,
    /// Either side consumed one half of the duplex stream.
    HalfClose,
    /// Both directions closed normally.
    Close,
    /// A typed failure terminated the flow.
    Fail,
}

/// Lifecycle state for one captured TCP flow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlowState {
    /// The immutable target passed admission.
    TargetBound(FlowId),
    /// The onion stream open handshake is in progress.
    Opening(FlowId),
    /// Both directions are open.
    Established(FlowId),
    /// At least one direction is closed while the other may still transfer bytes.
    HalfClosed(FlowId),
    /// The flow completed normally.
    Closed(FlowId),
    /// The flow failed closed.
    Failed(FlowId),
}

impl FlowState {
    /// Return the immutable five-tuple projection used by this TCP-only milestone.
    pub const fn id(self) -> FlowId {
        match self {
            Self::TargetBound(id)
            | Self::Opening(id)
            | Self::Established(id)
            | Self::HalfClosed(id)
            | Self::Closed(id)
            | Self::Failed(id) => id,
        }
    }

    /// Apply one deterministic lifecycle event.
    pub fn transition(self, event: FlowEvent) -> Result<Self, FlowTransitionError> {
        let id = self.id();
        match (self, event) {
            (Self::TargetBound(_), FlowEvent::Open) => Ok(Self::Opening(id)),
            (Self::Opening(_), FlowEvent::Establish) => Ok(Self::Established(id)),
            (Self::Established(_), FlowEvent::HalfClose) => Ok(Self::HalfClosed(id)),
            (Self::Established(_) | Self::HalfClosed(_), FlowEvent::Close) => Ok(Self::Closed(id)),
            (
                Self::TargetBound(_)
                | Self::Opening(_)
                | Self::Established(_)
                | Self::HalfClosed(_),
                FlowEvent::Fail,
            ) => Ok(Self::Failed(id)),
            (state, rejected) => Err(FlowTransitionError {
                state,
                event: rejected,
            }),
        }
    }

    /// Return whether no further flow transition is permitted.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Closed(_) | Self::Failed(_))
    }
}

/// Cause of one captured flow being reset.
///
/// Every runtime reset carries one value of this type, so a reset without a cause cannot be
/// constructed; `crate::reset` states which value names which end. Variants carry no packet
/// contents and no flow addresses.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "detail")]
pub enum FlowResetReason {
    /// The TCP handshake did not complete within the flow timeout.
    HandshakeExpired,
    /// The Onion stream did not open before the client was silent for the flow idle timeout.
    OpenExpired,
    /// The client was silent for the flow idle timeout while its open stream was healthy: no
    /// failure of its circuit was observed and no frame of it was known missing.
    IdleTimeout,
    /// The client was silent for the flow idle timeout while a frame of its open stream was known
    /// missing: a later frame had arrived and was held behind it.
    StreamStalled,
    /// The captured client reset the connection.
    ClientReset,
    /// The TCP endpoint closed before both byte directions shut down.
    UngracefulClose,
    /// The Onion stream could not be opened; carries the node-layer diagnostic.
    OnionOpenFailed(String),
    /// The open Onion stream failed while carrying bytes.
    StreamFailed(String),
    /// The open Onion stream failed closed, at its far side or on a link it rides.
    StreamReset(String),
    /// The byte bridge stopped accepting commands before the flow closed.
    BridgeClosed,
    /// Gateway shutdown reset the flow.
    GatewayStopped,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> FlowId {
        FlowId {
            source: "100.64.0.2:41000".parse().expect("test source"),
            target: "93.184.216.34:443".parse().expect("test target"),
        }
    }

    #[test]
    fn legal_flow_trace_preserves_the_bound_target() {
        let id = flow();
        let events = [
            FlowEvent::Open,
            FlowEvent::Establish,
            FlowEvent::HalfClose,
            FlowEvent::Close,
        ];
        let terminal = events
            .into_iter()
            .try_fold(FlowState::TargetBound(id), FlowState::transition)
            .expect("legal flow trace");
        assert_eq!(terminal, FlowState::Closed(id));
        assert_eq!(terminal.id().target, id.target);
    }

    #[test]
    fn establishment_cannot_skip_stream_open() {
        let state = FlowState::TargetBound(flow());
        assert_eq!(
            state.transition(FlowEvent::Establish),
            Err(FlowTransitionError {
                state,
                event: FlowEvent::Establish,
            })
        );
    }

    #[test]
    fn terminal_flow_rejects_reopening() {
        let state = FlowState::Failed(flow());
        assert!(state.is_terminal());
        assert!(state.transition(FlowEvent::Open).is_err());
    }
}
