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
    /// The captured target passed admission and became immutable flow authority.
    BindTarget,
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
    /// Packets exist but target admission has not completed.
    Captured(FlowId),
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
            Self::Captured(id)
            | Self::TargetBound(id)
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
            (Self::Captured(_), FlowEvent::BindTarget) => Ok(Self::TargetBound(id)),
            (Self::TargetBound(_), FlowEvent::Open) => Ok(Self::Opening(id)),
            (Self::Opening(_), FlowEvent::Establish) => Ok(Self::Established(id)),
            (Self::Established(_), FlowEvent::HalfClose) => Ok(Self::HalfClosed(id)),
            (Self::Established(_) | Self::HalfClosed(_), FlowEvent::Close) => Ok(Self::Closed(id)),
            (
                Self::Captured(_)
                | Self::TargetBound(_)
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
            FlowEvent::BindTarget,
            FlowEvent::Open,
            FlowEvent::Establish,
            FlowEvent::HalfClose,
            FlowEvent::Close,
        ];
        let terminal = events
            .into_iter()
            .try_fold(FlowState::Captured(id), FlowState::transition)
            .expect("legal flow trace");
        assert_eq!(terminal, FlowState::Closed(id));
        assert_eq!(terminal.id().target, id.target);
    }

    #[test]
    fn route_open_cannot_skip_target_admission() {
        let state = FlowState::Captured(flow());
        assert_eq!(
            state.transition(FlowEvent::Open),
            Err(FlowTransitionError {
                state,
                event: FlowEvent::Open,
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
