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
            | Self::DetachedPayloadCleanupTimeout { .. }
            | Self::DataChannelSendCompletionTimeout { .. }
            | Self::DataChannelDeliveryTimeout { .. }
            | Self::TrackedPayloadCleanupTimeout { .. } => false,
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
