//! The send-acceptance class of every error: the proof obligation behind retrying a send.
//!
//! A data-plane send may be retried from fresh topology only if the refused attempt left no
//! remote effect. This module classifies **every** [`Error`] variant (a match with no wildcard
//! arm, so a new variant cannot compile unclassified) into exactly one [`SendClass`]:
//!
//! ```text
//! SendClass ≜ Deferrable(DeferralTrigger)   \* proved refused before backend acceptance
//!           | Ambiguous                     \* the backend may have accepted
//!           | Fatal                         \* not retried; no acceptance claim is made
//! DeferralTrigger ≜ LinkChange | CapacityRelease
//! ```
//!
//! # Acceptance
//!
//! A payload travels as frames; a receiver acts only on the whole payload (one frame, or the
//! reassembly of every chunk). `Claimed(f)` holds once the backend's `SendPermit` irrevocable
//! claim succeeded for frame `f` (`SendAcceptance::is_irrevocable`); only a claimed frame can
//! reach the wire. Hence
//!
//! ```text
//! Effect(m) ⇒ Claimed(last(m))        and, for a detached send,  Claimed(f) ⇒ Claimed(first(m))
//! PreAcceptance(e) ≜ an error e returned for m implies ¬Claimed(last(m)), so ¬Effect(m)
//! ```
//!
//! # Lemmas of the send path (`swarm::transport`)
//!
//! - **(P) Single publication.** A transfer publishes its result through one `oneshot` taken
//!   from an `Option` (`TransferCompletion::take_final`), so at most once. A detached transfer
//!   publishes `Ok(Succeeded)` when its first frame is admitted
//!   (`OutboundTransfer::take_frame_admission_result`), and that publication cannot fail after a
//!   successful claim: the detached admission moves `Pending → Irrevocable → Accepted`, and
//!   `Irrevocable → Cancelled` is a rollback taken only when the claim failed
//!   (`build_transport_send_permit`). So every `Err(e)` or `Ok(Cancelled)` a detached caller
//!   receives from the scheduler was published before its first frame was claimed; what a
//!   later frame does is never observed by that caller.
//! - **(C) Cancellable boundary.** `send_data_with_timeout` returns a pre-claim outcome
//!   (`ChunkSendProgress::Cancelled`, or `Ready(Err)` from the timeout arm) only after
//!   `SendAcceptance::try_cancel` succeeded or while `!is_irrevocable()`; once the claim
//!   succeeded it returns only through `await_irrevocable_send` / `complete_irrevocable_send`
//!   / `expire_irrevocable_send`, whose errors are backend errors and
//!   `DataChannelSendCompletionTimeout`.
//! - **(T) Transport contract.** `rings_transport::error::Error::SendPermitRevoked` is returned
//!   by every backend (dummy, native, browser queue) only when the permit's claim failed, i.e.
//!   "revoked before transport send admission".
//!
//! # Proof of `PreAcceptance` for each deferrable variant
//!
//! - `SwarmMissDidInTable`: produced by `inspect_outbound_preparation` and
//!   `connection_for_send`, before `submit`, so no transfer exists.
//! - `ConnectionAttemptSuperseded`: produced by `AdmittedConnection::ensure_current` in
//!   `prepare_outbound_transfer` (before `submit`), and by
//!   `ChunkSendCancelReason::resolve_initial`, which runs only for `before_first_frame` after
//!   a pre-claim outcome of (C).
//! - `TransportNotReady`: produced by `connection_for_send` (tracked, before `submit`) and by
//!   `resolve_initial` (as above).
//! - `Transport(SendPermitRevoked)`: produced by a backend's failed claim (T), surfaced through
//!   (C).
//! - `DataChannelSendQueueTimeout`: produced by the timeout arm of `send_data_with_timeout`,
//!   only after `try_cancel` succeeded (C).
//! - `OutboundTransferCapacityExceeded`, `OutboundTransferMemoryCapacityExceeded`,
//!   `OutboundTransferAdmissionTimeout`: produced by `reserve_outbound_capacity`, before
//!   `submit`.
//! - `OutboundFirstFrameAdmissionTimeout`: produced by `do_send_payload_detached_until` when
//!   preparation timed out before `submit`, or when the detached admission was cancelled from
//!   `Pending`, after which no claim can succeed.
//!
//! A variant produced after the first frame of a tracked transfer (a queue timeout or a revoked
//! permit on a later chunk) still leaves the last frame unclaimed, so the payload is never
//! reassembled. The deferrable set is therefore sound for both completion policies:
//!
//! ```text
//! Law (S1-class) : ∀e. send_class(e) = Deferrable(_) ⇒ PreAcceptance(e)
//! ```
//!
//! A detached `Ok(Cancelled)` is pre-acceptance by (P) and is carried as
//! [`SendDeferral::cancelled`]. `Ambiguous` collects the errors produced after a claim (by (C):
//! completion and delivery timeouts, cleanup timeouts after a claim, backend I/O); they are
//! never retried. `Fatal` makes no acceptance claim: its variants are not transient data-plane
//! conditions, so retrying them from fresh topology cannot change their outcome.

use std::fmt;

use rings_transport::error::Error as TransportError;

use super::Error;
use crate::dht::Did;

/// The acceptance class of an error returned by a data-plane send (see the module laws).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum SendClass {
    /// Proved refused before backend acceptance; a retry cannot duplicate a remote effect.
    Deferrable(DeferralTrigger),
    /// Produced after the backend may have accepted; the remote effect is unknown.
    Ambiguous,
    /// Not a transient data-plane condition; no acceptance claim is made.
    Fatal,
}

/// The class of event after which a deferred send may succeed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) enum DeferralTrigger {
    /// The hop's connection generation, its readiness, or the route changed.
    LinkChange,
    /// Local outbound capacity was released (or the link or route changed).
    CapacityRelease,
}

/// The cause of one deferral: a send refused before backend acceptance.
///
/// Inv: a value is built only from a detached `Cancelled` outcome or from an error whose
/// `SendClass` is `Deferrable`, so every value witnesses `PreAcceptance` (module laws).
#[derive(Debug)]
pub struct SendDeferral(DeferralCause);

/// The two shapes of a pre-acceptance refusal.
#[derive(Debug)]
enum DeferralCause {
    /// The detached transfer to `hop` was cancelled before its first frame was claimed.
    Cancelled {
        /// Link peer the transfer was bound to.
        hop: Did,
    },
    /// The send path refused with an error of class `Deferrable(trigger)`.
    Refused {
        /// The refusal.
        error: Box<Error>,
        /// The trigger its class names.
        trigger: DeferralTrigger,
    },
}

impl SendDeferral {
    /// A detached transfer to `hop` that completed `Cancelled`: pre-acceptance by lemma (P).
    pub(crate) const fn cancelled(hop: Did) -> Self {
        Self(DeferralCause::Cancelled { hop })
    }

    /// `Ok(deferral)` iff `error` is deferrable; otherwise the error itself, unchanged.
    ///
    /// Post: `Ok(d)` ⇒ `d.trigger() = t` where `error.send_class() = Deferrable(t)`.
    pub(crate) fn refused(error: Error) -> Result<Self, Error> {
        match error.send_class() {
            SendClass::Deferrable(trigger) => Ok(Self(DeferralCause::Refused {
                error: Box::new(error),
                trigger,
            })),
            SendClass::Ambiguous | SendClass::Fatal => Err(error),
        }
    }

    /// The event class after which this refusal may resolve.
    pub(crate) const fn trigger(&self) -> DeferralTrigger {
        match &self.0 {
            DeferralCause::Cancelled { .. } => DeferralTrigger::LinkChange,
            DeferralCause::Refused { trigger, .. } => *trigger,
        }
    }
}

impl fmt::Display for SendDeferral {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            DeferralCause::Cancelled { hop } => {
                write!(formatter, "send to {hop} cancelled before acceptance")
            }
            DeferralCause::Refused { error, .. } => write!(formatter, "{error}"),
        }
    }
}

impl Error {
    /// The acceptance class of this error (module table); total over every variant.
    ///
    /// A table, one row per class and, for `Fatal`, one group per error domain; it is laid out
    /// by hand because a formatted or-pattern takes a line per variant.
    #[rustfmt::skip]
    pub(crate) const fn send_class(&self) -> SendClass {
        match self {
            Self::ConnectionAttemptSuperseded { .. } | Self::TransportNotReady { .. }
            | Self::SwarmMissDidInTable(_) => SendClass::Deferrable(DeferralTrigger::LinkChange),
            Self::DataChannelSendQueueTimeout { .. }
            | Self::OutboundTransferCapacityExceeded { .. }
            | Self::OutboundTransferMemoryCapacityExceeded { .. }
            | Self::OutboundTransferAdmissionTimeout { .. }
            | Self::OutboundFirstFrameAdmissionTimeout { .. } => {
                SendClass::Deferrable(DeferralTrigger::CapacityRelease)
            }
            Self::DataChannelSendCompletionTimeout { .. } | Self::DataChannelDeliveryTimeout { .. }
            | Self::DetachedPayloadCleanupTimeout { .. } | Self::TrackedPayloadCleanupTimeout { .. }
            | Self::CancelledDetachedAdmissionPublishedSuccess => SendClass::Ambiguous,
            Self::Transport(error) => transport_send_class(error),
            #[cfg(all(feature = "wasm", target_family = "wasm"))]
            Self::IDBError(_) | Self::SerdeWasmBindgenError(_) => SendClass::Fatal,
            // cryptography
            Self::BlsInputLengthMismatch | Self::EccSerializeFailed | Self::EccDeserializeFailed
            | Self::CurveHasherInitFailed | Self::CurveHasherFailed | Self::EdDSAPublicKeyBadFormat
            | Self::ECDSAPublicKeyBadFormat | Self::Secp256k1PointLiftFailed | Self::ECDSAError(_)
            | Self::PublicKeyBadFormat | Self::PrivateKeyBadFormat | Self::InvalidPublicKey
            | Self::InvalidRecoverId(_) | Self::NonCanonicalSignature | Self::VerifySignatureFailed
            | Self::UnknownAccount | Self::InvalidAffineScalar
            // end-to-end frames
            | Self::E2eStreamIdMismatch { .. } | Self::E2eFrameSequenceMismatch { .. }
            | Self::E2eFrameReorderWindowExceeded { .. } | Self::E2eFrameSequenceOverflow
            | Self::E2eFrameAfterFinal | Self::E2eMissingFinalFrame
            | Self::E2ePublicKeyDidMismatch { .. }
            // transaction streams
            | Self::TransactionSequenceExhausted { .. } | Self::TransactionReplay { .. }
            | Self::TransactionSequenceFork { .. } | Self::TransactionSequenceStale { .. }
            | Self::TransactionReplayStreamCapacityExceeded { .. }
            | Self::TransactionReplayStateInvalid | Self::TransactionReplayPersistence { .. }
            // accounting and delegation
            | Self::OriginQuota(_) | Self::ServiceReceipt(_) | Self::ProvisionalEvidence(_)
            | Self::UnmarkedFrame | Self::DelegationReferenceUnresolved(_)
            | Self::DelegationExpired
            // link control
            | Self::LinkControlOutsideLink | Self::LinkControlRuntimeUnavailable
            | Self::LinkControlInFlightCapacity(_)
            // entries, inboxes and storage
            | Self::EntryKindNotEqual | Self::EntryDidNotEqual | Self::EntryNotOverwritable
            | Self::EntryNotAppendable | Self::EntryDotIndexOutOfBounds { .. } | Self::EntryNotLive
            | Self::EntryLifetimeExceedsMax | Self::EntryVersionAheadOfClock
            | Self::EntryPayloadExceedsMax | Self::RelayMessageNotAddressedToInbox
            | Self::RelayMessageNotCustom | Self::RelayMessageUnverifiable
            | Self::RelayMessageHeldAheadOfClock | Self::RelayMessageHeldOutsideSenderProof
            | Self::RelayMessageHoldStale | Self::RelayInboxDeltaExceedsCapacity
            | Self::RelayMessageHolderNotResponsible | Self::RelayInboxRegisterNotAllowed
            | Self::RelayInboxOperationNotAllowed | Self::RelayInboxWriterNotRecipient
            | Self::StorageCountOverflow | Self::StorageRedundancyMismatch { .. }
            | Self::InvalidCapacity | Self::StorageValueExceedsCapacity { .. }
            // encoding and payload bounds
            | Self::Encode | Self::Decode | Self::SerializeToString | Self::SerializeError
            | Self::Serialize(_) | Self::Deserialize(_) | Self::CodecSerialize(_)
            | Self::CodecDeserialize(_) | Self::BadHexInCache(_) | Self::BadCHexInCache
            | Self::BadArrayInCache(_) | Self::MessageEncryptionFailed(_)
            | Self::MessageDecryptionFailed(_) | Self::AeadWrappedKeyBlockCount { .. }
            | Self::InvalidMessage(_) | Self::NestedChunkMessage | Self::InvalidChunkMessage
            | Self::MessageTooLarge(_) | Self::MessageSizeOverflow
            | Self::PeerMaxMessageSizeTooSmall(_)
            // connection lifecycle
            | Self::PromiseStateTimeout | Self::AlreadyConnected
            | Self::PendingConnectionCapacityExceeded { .. }
            | Self::ConnectionCapacityExceeded { .. } | Self::PendingConnectionGenerationExhausted
            | Self::NotifyPredecessorOriginMismatch { .. }
            | Self::NotifyPredecessorOriginNotAdmitted { .. } | Self::SwarmConnectionLifecycleLock
            | Self::ShouldNotConnectSelf | Self::SwarmMissTransport(_) | Self::ConnectionNotFound
            | Self::InvalidTransport
            // local channels and schedulers
            | Self::ChannelSendMessageFailed | Self::ChannelRecvMessageFailed(_)
            | Self::OutboundSchedulerRuntimeUnavailable | Self::LockPoisoned
            // inbound processing
            | Self::InboundMailboxCapacityExceeded { .. }
            | Self::InboundMailboxMemoryCapacityExceeded { .. }
            | Self::InboundPeerCapacityExceeded { .. }
            | Self::InboundPeerMemoryCapacityExceeded { .. } | Self::InboundMailboxClosed
            | Self::InboundMailboxRuntimeUnavailable | Self::InboundActorInvariantViolation
            | Self::InboundValidationFailed { .. } | Self::InboundValidationTimeout { .. }
            | Self::InboundCallbackFailed { .. } | Self::InboundProcessingTimeout { .. }
            | Self::InboundTimerUnavailable { .. }
            // routing
            | Self::PeerRingInvalidAction | Self::FailedToReadSuccessors
            | Self::SuccessorIndexOutOfBounds { .. } | Self::FailedToWriteSuccessors
            | Self::PeerRingUnexpectedAction(_) | Self::InvalidNextHop
            | Self::RelayHopBudgetExhausted | Self::RelayHopBudgetAboveMax(_) | Self::NoNextHop
            | Self::ReroutingExhausted { .. }
            // host
            | Self::ServiceIOError(_) | Self::JsError(_) => SendClass::Fatal,
        }
    }
}

/// The acceptance class of a backend error, total over its (feature-gated) variants.
///
/// The backend's gates follow this crate's features: `std` enables `native-webrtc`, `dummy`
/// enables `dummy`, and `wasm` enables `web-sys-webrtc`, so each arm is compiled exactly when
/// its variant exists.
///
/// Only `SendPermitRevoked` is proved pre-acceptance (lemma (T)). Backend I/O and the failures
/// of an irrevocable write are `Ambiguous`; configuration, pairing and pre-dispatch failures
/// are `Fatal`.
const fn transport_send_class(error: &rings_transport::error::Error) -> SendClass {
    match error {
        TransportError::SendPermitRevoked => SendClass::Deferrable(DeferralTrigger::LinkChange),
        TransportError::Io(_) | TransportError::MessageNotDelivered(_) => SendClass::Ambiguous,
        #[cfg(feature = "std")]
        TransportError::Webrtc(_)
        | TransportError::NativeSendCompletionTimeout { .. }
        | TransportError::NativeSendTask(_)
        | TransportError::NativeSendPanic(_) => SendClass::Ambiguous,
        #[cfg(feature = "std")]
        TransportError::NativeSendRuntimeUnavailable
        | TransportError::NativeConnectionCloseTask(_)
        | TransportError::NativeConnectionRetirementTimeout { .. } => SendClass::Fatal,
        #[cfg(all(feature = "wasm", target_family = "wasm"))]
        TransportError::WebSysWebrtc(_) => SendClass::Ambiguous,
        #[cfg(feature = "dummy")]
        TransportError::DummyIrrevocableSendTaskStopped => SendClass::Ambiguous,
        #[cfg(feature = "dummy")]
        TransportError::DummyConnectionRetiredBeforeDispatch
        | TransportError::DummyRemoteConnectionUnavailable
        | TransportError::DummyRemoteConnectionClosed => SendClass::Fatal,
        TransportError::Codec(_)
        | TransportError::IceServer(_)
        | TransportError::DataChannelOpen(_)
        | TransportError::Timer(_)
        | TransportError::DataChannelMessage(_)
        | TransportError::SendByteCountOverflow
        | TransportError::WebrtcLocalSdpGenerationError(_)
        | TransportError::WebrtcUdpPortRange(_)
        | TransportError::ConnectionAlreadyExists(_)
        | TransportError::ConnectionNotFound(_)
        | TransportError::ConnectionReleased(_)
        | TransportError::RwLockWrite(_)
        | TransportError::RwLockRead(_)
        | TransportError::RoundRobinPoolEmpty => SendClass::Fatal,
    }
}

#[cfg(test)]
mod tests {
    use super::DeferralTrigger;
    use super::SendClass;
    use super::SendDeferral;
    use crate::dht::Did;
    use crate::error::Error;

    /// Law (S1-class): a `SendDeferral` is built exactly from the deferrable errors, keeping the
    /// trigger of their class; every other error is returned unchanged.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_a_deferral_is_built_exactly_from_deferrable_errors() {
        let peer = Did::from(7_u32);
        let deferrable = [
            (
                Error::SwarmMissDidInTable(peer),
                DeferralTrigger::LinkChange,
            ),
            (
                Error::Transport(rings_transport::error::Error::SendPermitRevoked),
                DeferralTrigger::LinkChange,
            ),
            (
                Error::OutboundTransferCapacityExceeded { peer, capacity: 1 },
                DeferralTrigger::CapacityRelease,
            ),
        ];
        for (error, trigger) in deferrable {
            let described = error.to_string();
            let deferral = SendDeferral::refused(error).expect("a deferrable error defers");
            assert_eq!(deferral.trigger(), trigger);
            assert_eq!(deferral.to_string(), described);
        }
        let surfaced = [
            Error::DataChannelSendCompletionTimeout {
                peer,
                timeout_ms: 1,
                bytes: 1,
                context: "test",
            },
            Error::NoNextHop,
            Error::MessageTooLarge(1),
        ];
        for error in surfaced {
            let class = error.send_class();
            assert!(!matches!(class, SendClass::Deferrable(_)), "{error:?}");
            let returned = SendDeferral::refused(error).expect_err("a surfaced error is kept");
            assert_eq!(returned.send_class(), class);
        }
        assert_eq!(
            SendDeferral::cancelled(peer).trigger(),
            DeferralTrigger::LinkChange
        );
    }

    /// The exhaustion error is never itself retried.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_rerouting_exhaustion_is_fatal() {
        let peer = Did::from(7_u32);
        let exhausted = Error::ReroutingExhausted {
            deferrals: 1,
            last: SendDeferral::cancelled(peer),
        };
        assert_eq!(exhausted.send_class(), SendClass::Fatal);
    }
}
