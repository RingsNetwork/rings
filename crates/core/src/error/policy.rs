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

    /// Whether this error is a view refusal (#1151): the message was routed under a view that
    /// no longer matches this node's, which views converge from on their own, so the inbound
    /// actor logs it at `debug` and reports the step done.
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

    /// Whether this error is a view refusal (#1151) or expected transient multi-hop routing /
    /// topology-churn rejection (#1316) that the transport callback boundary should log at
    /// `debug` rather than surfacing as an `ERROR` line.
    pub(crate) fn is_transient_inbound_rejection(&self) -> bool {
        if self.is_view_refusal() {
            return true;
        }
        match self {
            Self::RelayDestinationUnreachable { .. }
            | Self::RelayPathRepeatedHop { .. }
            | Self::RelayPathHopUnlinked { .. }
            | Self::RelayHopBudgetExhausted
            | Self::TransactionSequenceStale { .. }
            | Self::TransactionReplay { .. }
            | Self::SwarmMissDidInTable(_)
            | Self::SwarmMissTransport(_)
            | Self::NotifyPredecessorOriginNotAdmitted { .. }
            | Self::ConnectionAttemptCancelled { .. }
            | Self::ConnectionAttemptSuperseded { .. }
            | Self::OutboundTransferCancelled { .. } => true,
            Self::InvalidMessage(reason) => {
                reason == "probe transcript messages must be delivered directly without relaying"
            }
            _ => false,
        }
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
            | Self::ChannelSendCompletionTimeout { .. }
            | Self::ChannelDeliveryTimeout { .. }
            | Self::TrackedPayloadCleanupTimeout { .. }
            | Self::DelegationExpiresWithinMargin => false,
            Self::Transport(rings_transport::error::Error::SendPermitRevoked) => false,
            Self::TransportNotReady { state, .. } => matches!(
                state,
                rings_transport::core::transport::TransportConnectionState::Failed
                    | rings_transport::core::transport::TransportConnectionState::Closed
            ),
            _ => true,
        }
    }
}
