#[cfg(test)]
use super::DeferralTrigger;
use super::Error;
use super::SendClass;

impl Error {
    /// Wrap an unexpected [`crate::dht::PeerRingAction`] in [`Error::PeerRingUnexpectedAction`].
    pub(crate) fn unexpected_peer_ring_action(action: crate::dht::PeerRingAction) -> Self {
        Self::PeerRingUnexpectedAction(Box::new(action))
    }

    /// True when local pre-send admission or memory capacity is exhausted.
    #[cfg(test)]
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
    pub(crate) const fn is_transient_inbound_rejection(&self) -> bool {
        self.is_view_refusal()
            || matches!(
                self,
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
                    | Self::OutboundTransferCancelled { .. }
                    | Self::NonDirectProbeTranscript
            )
    }

    /// Whether this error is a transient routing or peer-departure refusal (#1321): the target
    /// peer left the routing table, rotated or cancelled its connection generation, or its
    /// transport transitioned out of a ready state before an outbound follow-up or maintenance
    /// send could leave.
    pub(crate) const fn is_transient_routing_refusal(&self) -> bool {
        matches!(
            self,
            Self::SwarmMissDidInTable(_)
                | Self::SwarmMissTransport(_)
                | Self::TransportNotReady { .. }
                | Self::ConnectionAttemptSuperseded { .. }
                | Self::ConnectionAttemptCancelled { .. }
                | Self::OutboundTransferCancelled { .. }
                | Self::RelayDestinationUnreachable { .. }
                | Self::RelayPathHopUnlinked { .. }
        )
    }

    /// Whether this error should degrade peer quality through `FailedToSend`.
    ///
    /// Exhaustive over every [`Error`] variant so no newly added pre-acceptance, local-runtime,
    /// shutdown, or caller-validation error can silently penalize a remote peer's quality score.
    #[rustfmt::skip]
    pub(crate) const fn records_peer_send_failure(&self) -> bool {
        match self {
            Self::TransportNotReady { state, .. } => matches!(
                state,
                rings_transport::core::transport::TransportConnectionState::Failed
                    | rings_transport::core::transport::TransportConnectionState::Closed
            ),
            Self::Transport(error) => error.blames_remote_peer_on_send(),
            Self::PeerMaxMessageSizeTooSmall(_) | Self::HandshakeAboveMax(_) => true,
            // pre-acceptance deferrable refusals
            Self::ConnectionAttemptSuperseded { .. } | Self::SwarmMissDidInTable(_)
            | Self::OutboundTransferCancelled { .. }
            | Self::OutboundTransferCapacityExceeded { .. }
            | Self::OutboundTransferMemoryCapacityExceeded { .. }
            | Self::OutboundTransferAdmissionTimeout { .. }
            | Self::ChannelSendQueueTimeout { .. }
            | Self::OutboundFirstFrameAdmissionTimeout { .. }
            // post-claim timeouts and local scheduler invariants
            | Self::ChannelSendCompletionTimeout { .. } | Self::ChannelDeliveryTimeout { .. }
            | Self::DetachedPayloadCleanupTimeout { .. } | Self::TrackedPayloadCleanupTimeout { .. }
            | Self::CancelledDetachedAdmissionPublishedSuccess
            | Self::DetachedSendAbandonedAfterClaim { .. }
            // wasm host storage
            | Self::BlsInputLengthMismatch | Self::EccSerializeFailed | Self::EccDeserializeFailed
            | Self::CurveHasherInitFailed | Self::CurveHasherFailed | Self::EdDSAPublicKeyBadFormat
            | Self::ECDSAPublicKeyBadFormat | Self::Secp256k1PointLiftFailed | Self::ECDSAError(_)
            | Self::PublicKeyBadFormat | Self::PrivateKeyBadFormat | Self::InvalidPublicKey
            | Self::InvalidRecoverId(_) | Self::NonCanonicalSignature | Self::VerifySignatureFailed
            | Self::UnknownAccount | Self::InvalidAffineScalar | Self::IdentityElement
            | Self::ZeroScalar | Self::InvalidSignatureLength { .. }
            // end-to-end frames
            | Self::E2eStreamIdMismatch { .. } | Self::E2eFrameSequenceMismatch { .. }
            | Self::E2eFrameReorderWindowExceeded { .. } | Self::E2eFrameSequenceOverflow
            | Self::E2eFrameAfterFinal | Self::E2eMissingFinalFrame
            | Self::E2ePublicKeyDidMismatch { .. } | Self::E2eStreamStalled { .. }
            // transaction streams
            | Self::TransactionSequenceExhausted { .. } | Self::TransactionReplay { .. }
            | Self::TransactionSequenceFork { .. } | Self::TransactionSequenceStale { .. }
            | Self::TransactionReplayStreamCapacityExceeded { .. }
            | Self::TransactionReplayStateInvalid | Self::TransactionReplayPersistence { .. }
            | Self::TransactionReplayStreamUnavailable { .. }
            | Self::TransactionReplayUnscheduled(_)
            // accounting and delegation
            | Self::OriginQuota(_) | Self::ServiceReceipt(_) | Self::ProvisionalEvidence(_)
            | Self::UnmarkedFrame | Self::IncompatibleWireVersion(_)
            | Self::DelegationReferenceUnresolved(_)
            | Self::DelegationNotYetValid | Self::DelegationExpired
            | Self::DelegateeKeyMismatch { .. } | Self::DelegationExpiresWithinMargin
            // link control
            | Self::LinkControlOutsideLink | Self::LinkControlInFlightCapacity(_)
            | Self::DetachedRuntimeUnavailable(_)
            // entries, inboxes and storage
            | Self::EntryKindNotEqual | Self::EntryDidNotEqual | Self::EntryNotOverwritable
            | Self::DataEntryWriteUnauthorized
            | Self::EntryDotIndexOutOfBounds { .. } | Self::EntryNotLive
            | Self::EntryLifetimeExceedsMax | Self::EntryVersionAheadOfClock
            | Self::EntryPayloadExceedsMax | Self::EntryTombstonesExceedMax { .. }
            | Self::RemovalCapacityExhausted { .. }
            | Self::RelayMessageNotAddressedToInbox
            | Self::RelayMessageNotCustom | Self::RelayMessageUnverifiable
            | Self::RelayMessageHeldAheadOfClock | Self::RelayMessageHeldOutsideSenderProof
            | Self::RelayMessageHoldStale | Self::RelayInboxDeltaExceedsCapacity
            | Self::RelayMessageHolderNotResponsible | Self::RelayInboxRegisterNotAllowed
            | Self::RelayInboxOperationNotAllowed | Self::RelayInboxWriterNotRecipient
            | Self::RelayStorageUnavailable | Self::RelayHolderUnavailable
            | Self::RelayCarrierNotHeldHere | Self::RelayInboxReportFromNonMember(_)
            | Self::RelayHoldsUnacknowledgedFull
            | Self::TransactionReplayStorageUnavailable
            | Self::StorageCountOverflow | Self::StorageRedundancyMismatch { .. }
            | Self::InvalidCapacity | Self::StorageValueExceedsCapacity { .. }
            | Self::StorageBudgetExhausted { .. } | Self::StorageWorkUnscheduled(_)
            | Self::StorageRecordUndecodable(_) | Self::StorageRecordMissing(_)
            | Self::StorageDirectoryLocked(_)
            | Self::RelayCustodyFull { .. }
            // encoding and payload bounds
            | Self::Encode | Self::Decode | Self::SerializeToString | Self::SerializeError
            | Self::Serialize(_) | Self::Deserialize(_) | Self::CodecSerialize(_)
            | Self::CodecDeserialize(_) | Self::BadHexInCache(_) | Self::BadCHexInCache
            | Self::BadArrayInCache(_) | Self::MessageEncryptionFailed(_)
            | Self::MessageDecryptionFailed(_) | Self::AeadWrappedKeyBlockCount { .. }
            | Self::InvalidMessage(_) | Self::NonDirectProbeTranscript
            | Self::NestedChunkMessage | Self::InvalidChunkMessage
            | Self::MessageTooLarge(_) | Self::MessageSizeOverflow
            // connection lifecycle
            | Self::AlreadyConnected
            | Self::ConnectionAttemptCancelled { .. }
            | Self::PendingConnectionCapacityExceeded { .. }
            | Self::ConnectionCapacityExceeded { .. } | Self::PendingConnectionGenerationExhausted
            | Self::NotifyPredecessorOriginMismatch { .. }
            | Self::NotifyPredecessorOriginNotAdmitted { .. } | Self::SwarmConnectionLifecycleLock
            | Self::ShouldNotConnectSelf | Self::SwarmMissTransport(_) | Self::ConnectionNotFound
            // local channels and schedulers
            | Self::ChannelSendMessageFailed | Self::ChannelRecvMessageFailed(_)
            | Self::OutboundSchedulerRuntimeUnavailable | Self::LockPoisoned
            | Self::ApplicationInboxClaimed | Self::SwarmStopped | Self::SwarmClosed
            // inbound processing
            | Self::InboundMailboxMemoryCapacityExceeded { .. }
            | Self::InboundPeerMemoryCapacityExceeded { .. } | Self::InboundMailboxClosed
            | Self::InboundMailboxRuntimeUnavailable | Self::InboundActorInvariantViolation
            | Self::InboundLaneMismatch
            // routing
            | Self::PeerRingInvalidAction | Self::FailedToReadSuccessors
            | Self::SuccessorIndexOutOfBounds { .. } | Self::FailedToWriteSuccessors
            | Self::PeerRingUnexpectedAction(_) | Self::InvalidNextHop
            | Self::RelayHopBudgetExhausted | Self::RelayHopBudgetAboveMax(_)
            | Self::RelayPathAboveMax(_) | Self::RelayPathHopUnlinked { .. }
            | Self::RelayPathRepeatedHop { .. } | Self::RelaySignalingPathMismatch
            | Self::RelayDestinationUnreachable { .. }
            | Self::ReroutingExhausted { .. } | Self::SingleAttemptRefused { .. }
            | Self::ReroutingStopped
            // host
            | Self::ServiceIOError(_) | Self::JsError(_) => false,
            #[cfg(all(feature = "wasm", target_family = "wasm"))]
            Self::IDBError(_) | Self::SerdeWasmBindgenError(_) | Self::IdbAccessClockMissing
            | Self::IdbAccessClockExhausted(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use rings_transport::core::transport::TransportConnectionState;

    use super::Error;
    use crate::dht::Did;

    #[test]
    fn test_transient_routing_refusal_classification() {
        let peer = Did::from(1_u32);
        let transient = [
            Error::SwarmMissDidInTable(peer),
            Error::SwarmMissTransport(peer),
            Error::TransportNotReady {
                state: TransportConnectionState::Disconnected,
                channel_open: true,
            },
            Error::ConnectionAttemptSuperseded {
                peer,
                generation: 1,
            },
            Error::ConnectionAttemptCancelled {
                peer,
                generation: 2,
            },
            Error::OutboundTransferCancelled { peer },
            Error::RelayDestinationUnreachable { destination: peer },
            Error::RelayPathHopUnlinked { hop: peer },
        ];
        for error in transient {
            assert!(
                error.is_transient_routing_refusal(),
                "expected transient routing refusal for {error:?}"
            );
        }

        let non_transient = [
            Error::LockPoisoned,
            Error::PeerRingInvalidAction,
            Error::VerifySignatureFailed,
            Error::ServiceIOError(std::io::Error::other("io")),
        ];
        for error in non_transient {
            assert!(
                !error.is_transient_routing_refusal(),
                "expected non-transient error for {error:?}"
            );
        }
    }

    #[test]
    fn test_pre_acceptance_and_local_runtime_errors_do_not_blame_peer() {
        let peer = Did::from(2_u32);
        let non_peer_faults = [
            Error::Transport(rings_transport::error::Error::LinkCreditClosed(
                peer.to_string(),
            )),
            Error::Transport(rings_transport::error::Error::FrameTooLarge {
                size: 1024,
                max: 512,
            }),
            Error::SwarmMissDidInTable(peer),
            Error::OutboundTransferCancelled { peer },
            Error::SwarmStopped,
            Error::SwarmClosed,
            Error::LockPoisoned,
            Error::NonDirectProbeTranscript,
        ];
        for error in non_peer_faults {
            assert!(
                !error.records_peer_send_failure(),
                "expected {error:?} not to record peer send failure"
            );
        }
        assert!(Error::NonDirectProbeTranscript.is_transient_inbound_rejection());
        assert!(
            !Error::InvalidMessage("arbitrary invalid message".to_string())
                .is_transient_inbound_rejection()
        );
        assert!(
            Error::Transport(rings_transport::error::Error::MessageNotDelivered(
                "broken channel".to_string()
            ))
            .records_peer_send_failure()
        );
    }
}
