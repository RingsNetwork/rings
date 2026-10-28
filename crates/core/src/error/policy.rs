use super::DeferralTrigger;
use super::Error;
use super::SendClass;

impl Error {
    pub(crate) fn unexpected_peer_ring_action(action: crate::dht::PeerRingAction) -> Self {
        Self::PeerRingUnexpectedAction(Box::new(action))
    }

    /// True when local pre-send admission or memory capacity is exhausted.
    ///
    /// These failures happen before backend acceptance, so retrying cannot
    /// duplicate a send (`send_class` proves it per variant). Post-acceptance
    /// timeouts are deliberately excluded: their remote outcome is ambiguous
    /// even after the connection is retired.
    pub(crate) const fn is_local_send_backpressure(&self) -> bool {
        matches!(
            self.send_class(),
            SendClass::Deferrable(DeferralTrigger::CapacityRelease | DeferralTrigger::ChannelDrain)
        )
    }

    /// Whether a data-plane send should be retried from freshly computed topology.
    ///
    /// Law: `is_deferrable_data_plane_send(e) ⟺ send_class(e) = Deferrable(_)`, so every retried
    /// error is proved pre-acceptance.
    pub(crate) const fn is_deferrable_data_plane_send(&self) -> bool {
        matches!(self.send_class(), SendClass::Deferrable(_))
    }

    /// Whether this error is a view refusal (#1151): a node declines a relay operation its own
    /// view does not assign it, or a report from a node its view makes no member of the key.
    /// Views converge (`Law (Convergence)` of `dht::relay_holder`) and the sender acts again on
    /// its next pass, so a view refusal is the protocol's answer, not a fault.
    pub(crate) const fn is_view_refusal(&self) -> bool {
        matches!(
            self,
            Self::RelayHolderUnavailable
                | Self::RelayCarrierNotHeldHere
                | Self::RelayMessageHolderNotResponsible
                | Self::RelayStorageUnavailable
                | Self::RelayInboxReportFromNonMember(_)
        )
    }

    /// Whether this error should degrade peer quality through `FailedToSend`.
    pub(crate) const fn records_peer_send_failure(&self) -> bool {
        if self.is_local_send_backpressure() {
            return false;
        }

        match self {
            Self::ConnectionAttemptSuperseded { .. }
            | Self::OutboundSchedulerRuntimeUnavailable
            | Self::CancelledDetachedAdmissionPublishedSuccess
            | Self::DetachedSendAbandonedAfterClaim { .. }
            // rerouting outcomes: a placement's verdict, never a peer's send failure
            | Self::ReroutingExhausted { .. }
            | Self::SingleAttemptRefused { .. }
            | Self::ReroutingStopped
            | Self::DetachedPayloadCleanupTimeout { .. }
            | Self::DataChannelSendCompletionTimeout { .. }
            | Self::DataChannelDeliveryTimeout { .. }
            | Self::TrackedPayloadCleanupTimeout { .. }
            | Self::DelegationExpiresWithinMargin => false,
            Self::Transport(rings_transport::error::Error::SendPermitRevoked) => false,
            Self::TransportNotReady { state, .. } => matches!(
                state,
                rings_transport::core::transport::WebrtcConnectionState::Failed
                    | rings_transport::core::transport::WebrtcConnectionState::Closed
            ),
            _ => true,
        }
    }
}
