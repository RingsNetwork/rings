use super::Error;

impl Error {
    pub(crate) fn unexpected_peer_ring_action(action: crate::dht::PeerRingAction) -> Self {
        Self::PeerRingUnexpectedAction(Box::new(action))
    }

    /// True when local pre-send admission or memory capacity is exhausted.
    ///
    /// These failures happen before backend acceptance, so retrying cannot
    /// duplicate a send. Post-acceptance timeouts are deliberately excluded:
    /// their remote outcome is ambiguous even after the connection is retired.
    pub(crate) const fn is_local_send_backpressure(&self) -> bool {
        matches!(
            self,
            Self::DataChannelSendQueueTimeout { .. }
                | Self::OutboundTransferCapacityExceeded { .. }
                | Self::OutboundTransferMemoryCapacityExceeded { .. }
                | Self::OutboundTransferAdmissionTimeout { .. }
                | Self::OutboundFirstFrameAdmissionTimeout { .. }
        )
    }

    /// Whether a data-plane send should be retried from freshly computed topology.
    pub(crate) const fn is_deferrable_data_plane_send(&self) -> bool {
        self.is_local_send_backpressure()
            || matches!(
                self,
                Self::ConnectionAttemptSuperseded { .. }
                    | Self::RTCDataChannelStateNotOpen
                    | Self::TransportNotReady { .. }
                    | Self::SwarmMissDidInTable(_)
                    | Self::Transport(rings_transport::error::Error::SendPermitRevoked)
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
