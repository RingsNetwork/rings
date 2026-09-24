use super::CallbackError;

/// Errors collections in ring-core.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The BLS aggregate verifier received different numbers of hashes and public keys.
    #[error("BLS hash and public key counts differ")]
    BlsInputLengthMismatch,
    /// Serialize affine failed
    #[error("Serialize affine failed")]
    EccSerializeFailed,
    /// desrialize affine failed
    #[error("desrialize affine failed")]
    EccDeserializeFailed,
    /// Failed to initialize Curve hasher
    #[error("Failed to initialize Curve hasher")]
    CurveHasherInitFailed,
    /// Failed to hash data into cruve
    #[error("Failed to hash data into cruve")]
    CurveHasherFailed,

    /// Ed25519/EdDSA pubkey bad format
    #[error("Ed25519/EdDSA pubkey bad format")]
    EdDSAPublicKeyBadFormat,

    /// Secp256k1/ECDSA pubkey bad format
    #[error("Secp256k1/ECDSA pubkey bad format")]
    ECDSAPublicKeyBadFormat,

    /// Failed to lift encoded plaintext into a secp256k1 point
    #[error("Failed to lift encoded plaintext into a secp256k1 point")]
    Secp256k1PointLiftFailed,

    /// E2E stream id mismatch: expected {expected}, actual {actual}
    #[error("E2E stream id mismatch: expected {expected}, actual {actual}")]
    E2eStreamIdMismatch {
        /// Stream ID expected by the decryptor.
        expected: uuid::Uuid,
        /// Stream ID carried by the frame.
        actual: uuid::Uuid,
    },

    /// E2E frame sequence mismatch: expected {expected}, actual {actual}
    #[error("E2E frame sequence mismatch: expected {expected}, actual {actual}")]
    E2eFrameSequenceMismatch {
        /// Sequence number expected by the decryptor.
        expected: u64,
        /// Sequence number carried by the frame.
        actual: u64,
    },

    /// E2E frame sequence is outside the accepted reorder window.
    #[error(
        "E2E frame sequence {actual} exceeds reorder window {window} from next sequence {next_sequence}"
    )]
    E2eFrameReorderWindowExceeded {
        /// Next contiguous sequence number expected by the decryptor.
        next_sequence: u64,
        /// Sequence number carried by the frame.
        actual: u64,
        /// Maximum accepted gap ahead of the next sequence.
        window: u64,
    },

    /// E2E frame sequence counter overflowed
    #[error("E2E frame sequence counter overflowed")]
    E2eFrameSequenceOverflow,

    /// A sender exhausted the sequence space for one destination-scoped transaction stream.
    #[error("Transaction sequence exhausted for {key:?}")]
    TransactionSequenceExhausted {
        /// Stream whose counter cannot advance without wrapping.
        key: crate::message::StreamKey,
    },

    /// An exact signed transaction was already admitted at the final destination.
    #[error("Transaction sequence {sequence} is a replay for {key:?}")]
    TransactionReplay {
        /// Destination-scoped stream.
        key: crate::message::StreamKey,
        /// Replayed sequence.
        sequence: u64,
    },

    /// Two different signed transactions claimed one stream sequence.
    #[error("Transaction sequence {sequence} forks {key:?}")]
    TransactionSequenceFork {
        /// Destination-scoped stream.
        key: crate::message::StreamKey,
        /// Conflicting sequence.
        sequence: u64,
        /// Previously retained and newly presented transaction digests.
        evidence: Box<crate::message::TransactionForkEvidence>,
    },

    /// A transaction sequence fell below the retained replay window.
    #[error(
        "Transaction sequence {sequence} is stale for {key:?}; retained window starts at {retained_min}"
    )]
    TransactionSequenceStale {
        /// Destination-scoped stream.
        key: crate::message::StreamKey,
        /// Rejected sequence.
        sequence: u64,
        /// Lowest sequence whose digest may still be retained.
        retained_min: u64,
    },

    /// A new replay stream would exceed the fail-closed stream table bound.
    #[error("Transaction replay stream capacity {capacity} exceeded")]
    TransactionReplayStreamCapacityExceeded {
        /// Maximum sender streams or receiver streams retained by one runtime.
        capacity: usize,
    },

    /// The versioned replay snapshot violates its structural bounds.
    #[error("Transaction replay state is invalid")]
    TransactionReplayStateInvalid,

    /// Durable replay state could not be loaded or committed.
    #[error("Transaction replay persistence failed during {operation}: {source}")]
    TransactionReplayPersistence {
        /// Persistence operation that failed.
        operation: &'static str,
        /// Storage failure retained as the source.
        #[source]
        source: Box<Error>,
    },

    /// Final-destination origin-quota admission failed.
    #[error(transparent)]
    OriginQuota(#[from] crate::message::OriginQuotaError),

    /// A provisional service receipt or probe transcript is invalid.
    #[error(transparent)]
    ServiceReceipt(#[from] crate::message::ServiceReceiptError),

    /// Bounded provisional-evidence state rejected a transition.
    #[error(transparent)]
    ProvisionalEvidence(#[from] rings_measure::EvidenceError),

    /// The frame carries neither the payload marker nor the link-control marker.
    #[error("Frame carries no rings marker")]
    UnmarkedFrame,

    /// A payload that travels outside any link referenced a session instead of carrying it.
    #[error("Delegation reference {0:?} cannot be resolved outside the link that announced it")]
    DelegationReferenceUnresolved(crate::delegation::DelegationDigest),

    /// A link-control frame was decoded where a payload was expected: outside any link.
    #[error("Link-control frame is meaningful only on the connection it arrived on")]
    LinkControlOutsideLink,

    /// No runtime is current to carry a link-control send detached from the read loop.
    #[error("Link-control send needs a runtime to run detached")]
    LinkControlRuntimeUnavailable,

    /// The peer already has the most link-control sends this end keeps in flight for it.
    #[error("Link-control sends in flight to {0} are at capacity")]
    LinkControlInFlightCapacity(crate::dht::Did),

    /// E2E frame received after the authenticated final frame
    #[error("E2E frame received after the authenticated final frame")]
    E2eFrameAfterFinal,

    /// E2E stream is missing the authenticated final frame
    #[error("E2E stream is missing the authenticated final frame")]
    E2eMissingFinalFrame,

    /// E2E public key resolves to {actual}, expected {expected}
    #[error("E2E public key resolves to {actual}, expected {expected}")]
    E2ePublicKeyDidMismatch {
        /// DID expected by the signed message context.
        expected: crate::dht::Did,
        /// DID derived from the supplied public key.
        actual: crate::dht::Did,
    },

    /// Secp256r1/ECDSA Error: {0}
    #[error("Secp256r1/ECDSA Error: {0}")]
    ECDSAError(#[from] ecdsa::Error),

    /// ECDSA or EdDSA pubkey bad format
    #[error("ECDSA or EdDSA pubkey bad format")]
    PublicKeyBadFormat,

    /// private bad format
    #[error("private bad format")]
    PrivateKeyBadFormat,

    /// Invalid Transport
    #[error("Invalid Transport")]
    InvalidTransport,

    /// InvalidPublicKey
    #[error("InvalidPublicKey")]
    InvalidPublicKey,

    /// Entry kind not equal when overwriting
    #[error("Entry kind not equal when overwriting")]
    EntryKindNotEqual,

    /// Did of Entry not equal
    #[error("Did of Entry not equal")]
    EntryDidNotEqual,

    /// The type of Entry is not allowed to be overwritten
    #[error("The type of Entry is not allowed to be overwritten")]
    EntryNotOverwritable,

    /// The type of Entry is not allowed to be appended
    #[error("The type of Entry is not allowed to be appended")]
    EntryNotAppendable,

    /// Entry dot index {index} is out of bounds
    #[error("Entry dot index {index} is out of bounds")]
    EntryDotIndexOutOfBounds {
        /// Dot index that could not be represented.
        index: usize,
    },

    /// Entry carries no retention bound or its retention bound has elapsed
    #[error("Entry carries no retention bound or its retention bound has elapsed")]
    EntryNotLive,

    /// Entry retention bound exceeds the maximum time-to-live
    #[error("Entry retention bound exceeds the maximum time-to-live")]
    EntryLifetimeExceedsMax,

    /// Entry version logical time is beyond the accepted clock skew
    #[error("Entry version logical time is beyond the accepted clock skew")]
    EntryVersionAheadOfClock,

    /// Entry payload exceeds the per-payload size bound
    #[error("Entry payload exceeds the per-payload size bound")]
    EntryPayloadExceedsMax,

    /// Relay inbox element is not addressed to the peer the inbox is kept for
    #[error("Relay inbox element is not addressed to the peer the inbox is kept for")]
    RelayMessageNotAddressedToInbox,

    /// Relay inbox element does not carry a custom message
    #[error("Relay inbox element does not carry a custom message")]
    RelayMessageNotCustom,

    /// Relay inbox element holder signature does not verify inside this overlay
    #[error("Relay inbox element holder signature does not verify inside this overlay")]
    RelayMessageUnverifiable,

    /// Relay inbox element was held ahead of the receiver's clock
    #[error("Relay inbox element was held ahead of the receiver's clock")]
    RelayMessageHeldAheadOfClock,

    /// Relay inbox element payload does not verify as of its hold instant
    #[error("Relay inbox element payload does not verify as of its hold instant")]
    RelayMessageHeldOutsideSenderProof,

    /// Relay inbox hold arrived after its message's sender proof expired
    #[error("Relay inbox hold arrived after its message's sender proof expired")]
    RelayMessageHoldStale,

    /// Relay inbox delta carries more elements than the inbox keeps
    #[error("Relay inbox delta carries more elements than the inbox keeps")]
    RelayInboxDeltaExceedsCapacity,

    /// Relay inbox element was not held by the node responsible for its destination
    #[error("Relay inbox element was not held by the node responsible for its destination")]
    RelayMessageHolderNotResponsible,

    /// Relay inbox carrier must not carry a reset floor
    #[error("Relay inbox carrier must not carry a reset floor")]
    RelayInboxRegisterNotAllowed,

    /// Relay inbox operation is not a hold or a removal
    #[error("Relay inbox operation is not a hold or a removal")]
    RelayInboxOperationNotAllowed,

    /// Relay inbox removal was not issued by the inbox's recipient
    #[error("Relay inbox removal was not issued by the inbox's recipient")]
    RelayInboxWriterNotRecipient,

    /// A storage count or record length does not fit its interface width
    #[error("A storage count or record length does not fit its interface width")]
    StorageCountOverflow,

    /// A lock was poisoned by a panicking holder
    #[error("A lock was poisoned by a panicking holder")]
    LockPoisoned,

    /// A group element is the identity where `G ∖ {O}` is required
    #[error("group element is the identity")]
    IdentityElement,

    /// A scalar is zero where `Z_n^*` is required
    #[error("scalar is zero")]
    ZeroScalar,

    /// Affine rotation scalar must be greater than zero
    #[error("Affine rotation scalar must be greater than zero")]
    InvalidAffineScalar,

    /// Storage redundancy mismatch: transport configured {configured}, storage request uses {requested}
    #[error("Storage redundancy mismatch: transport configured {configured}, storage request uses {requested}")]
    StorageRedundancyMismatch {
        /// Redundancy configured on swarm transport for repair.
        configured: u16,
        /// Redundancy requested by the storage API const generic.
        requested: u16,
    },

    /// Encode a byte vector into a base58-check string, adds 4 bytes checksum
    #[error("Encode a byte vector into a base58-check string, adds 4 bytes checksum")]
    Encode,

    /// Decode base58-encoded with 4 bytes checksum string into a byte vector
    #[error("Decode base58-encoded with 4 bytes checksum string into a byte vector")]
    Decode,

    /// IOError
    #[error("IOError")]
    ServiceIOError(#[from] std::io::Error),

    /// Invalid hexadecimal id in directory cache
    #[error("Invalid hexadecimal id in directory cache")]
    BadHexInCache(#[from] hex::FromHexError),

    /// Invalid rustc hexadecimal id in directory cache
    #[error("Invalid rustc hexadecimal id in directory cache")]
    BadCHexInCache,

    /// Invalid hexadecimal id in directory cache
    #[error("Invalid hexadecimal id in directory cache")]
    BadArrayInCache(#[from] std::array::TryFromSliceError),

    /// JSON serialize toString error
    #[error("JSON serialize toString error")]
    SerializeToString,

    /// Serialization error
    #[error("Serialization error")]
    SerializeError,

    /// JSON serialization error
    #[error("JSON serialization error")]
    Serialize(#[source] serde_json::Error),

    /// JSON deserialization error
    #[error("JSON deserialization error")]
    Deserialize(#[source] serde_json::Error),

    /// Codec serialization error
    #[error("Codec serialization error")]
    CodecSerialize(#[source] rings_codec::Error),

    /// Codec deserialization error
    #[error("Codec deserialization error")]
    CodecDeserialize(#[source] rings_codec::Error),

    /// Unknown account
    #[error("Unknown account")]
    UnknownAccount,

    /// Failed on verify message signature
    #[error("Failed on verify message signature")]
    VerifySignatureFailed,

    /// ECDSA Invalid recover Id {0}
    #[error("ECDSA Invalid recover Id {0}")]
    InvalidRecoverId(u8),

    /// Signature encoding is valid but not in its canonical form.
    #[error("Signature is not canonical")]
    NonCanonicalSignature,

    /// promise timeout, state is not succeeded
    #[error("promise timeout, state is not succeeded")]
    PromiseStateTimeout,

    /// Found existing transport when answer offer from remote node
    #[error("Found existing transport when answer offer from remote node")]
    AlreadyConnected,

    /// Pending WebRTC connection capacity {capacity} is exhausted
    #[error("Pending WebRTC connection capacity {capacity} is exhausted")]
    PendingConnectionCapacityExceeded {
        /// Maximum number of concurrent pending peers.
        capacity: usize,
    },

    /// Logical peer connection capacity {capacity} is exhausted
    #[error("Logical peer connection capacity {capacity} is exhausted")]
    ConnectionCapacityExceeded {
        /// Maximum number of peers with a pending, admitting, or active connection.
        capacity: usize,
    },

    /// Pending WebRTC connection generation id space is exhausted.
    #[error("Pending WebRTC connection generation is exhausted")]
    PendingConnectionGenerationExhausted,

    /// Connection attempt {generation} for {peer} was replaced before setup completed.
    #[error("Connection attempt {generation} for {peer} was superseded")]
    ConnectionAttemptSuperseded {
        /// Peer whose connection generation changed.
        peer: crate::dht::Did,
        /// Generation that no longer owns the peer slot.
        generation: u64,
    },

    /// A predecessor notification claims a DID different from its signed origin.
    #[error("Notify predecessor DID {claimed} does not match relay origin {origin}")]
    NotifyPredecessorOriginMismatch {
        /// DID claimed by the notification body.
        claimed: crate::dht::Did,
        /// DID authenticated by the signed relay origin.
        origin: crate::dht::Did,
    },

    /// A predecessor notification originated from a peer without an admitted connection.
    #[error("Notify predecessor origin {origin} is not an admitted connection")]
    NotifyPredecessorOriginNotAdmitted {
        /// Authenticated origin that has no admitted connection generation.
        origin: crate::dht::Did,
    },

    /// Failed to access the swarm connection lifecycle state
    #[error("Failed to access the swarm connection lifecycle state")]
    SwarmConnectionLifecycleLock,

    /// You should not connect to yourself
    #[error("You should not connect to yourself")]
    ShouldNotConnectSelf,

    /// Send message through channel failed
    #[error("Send message through channel failed")]
    ChannelSendMessageFailed,

    /// The per-peer outbound scheduler has admitted its maximum transfer count.
    #[error("Outbound transfer capacity {capacity} exceeded for peer {peer}")]
    OutboundTransferCapacityExceeded {
        /// Peer whose scheduler is at capacity.
        peer: crate::dht::Did,
        /// Maximum transfers admitted across all scheduler states.
        capacity: usize,
    },

    /// The outbound scheduler cannot retain another payload within its byte budget.
    #[error(
        "Outbound transfer of {requested_bytes} bytes exceeds the remaining {capacity_bytes}-byte budget for peer {peer}"
    )]
    OutboundTransferMemoryCapacityExceeded {
        /// Peer whose scheduler would retain the payload.
        peer: crate::dht::Did,
        /// Bytes the transfer needs to retain.
        requested_bytes: usize,
        /// Total byte capacity of the exhausted budget.
        capacity_bytes: usize,
    },

    /// A detached send could not obtain bounded scheduler capacity in time.
    #[error(
        "Timed out after {timeout_ms}ms waiting for outbound transfer capacity for peer {peer}"
    )]
    OutboundTransferAdmissionTimeout {
        /// Peer whose scheduler capacity remained exhausted.
        peer: crate::dht::Did,
        /// Admission deadline in milliseconds.
        timeout_ms: u128,
    },

    /// A detached transfer did not admit its first frame before its deadline.
    #[error("Timed out after {timeout_ms}ms waiting to admit the first outbound frame for {peer}")]
    OutboundFirstFrameAdmissionTimeout {
        /// Peer whose scheduler lane did not admit the first frame.
        peer: crate::dht::Did,
        /// First-frame admission deadline in milliseconds.
        timeout_ms: u128,
    },

    /// A detached transfer did not stop within its post-deadline cleanup grace.
    #[error(
        "Detached payload cleanup for {peer} exceeded its {timeout_ms}ms grace after the first-frame deadline"
    )]
    DetachedPayloadCleanupTimeout {
        /// Peer whose exact connection generation was made send-terminal.
        peer: crate::dht::Did,
        /// Cleanup grace in milliseconds.
        timeout_ms: u128,
    },

    /// No Tokio runtime is available to host a native outbound scheduler.
    #[error("Outbound scheduler requires an active Tokio runtime")]
    OutboundSchedulerRuntimeUnavailable,

    /// A cancelled detached admission unexpectedly published send success.
    #[error("Cancelled detached outbound admission published success")]
    CancelledDetachedAdmissionPublishedSuccess,

    /// The inbound actor has admitted its maximum number of messages.
    #[error("Inbound mailbox capacity {capacity} exceeded")]
    InboundMailboxCapacityExceeded {
        /// Maximum queued and executing inbound messages.
        capacity: usize,
    },

    /// The inbound actor cannot retain another message within its byte budget.
    #[error(
        "Inbound message of {requested_bytes} bytes exceeds the {capacity_bytes}-byte mailbox budget"
    )]
    InboundMailboxMemoryCapacityExceeded {
        /// Bytes retained by the decoded message and its handler representation.
        requested_bytes: usize,
        /// Total mailbox byte capacity.
        capacity_bytes: usize,
    },

    /// One peer has exhausted its inbound message count allowance.
    #[error("Inbound peer {peer:?} capacity {capacity} exceeded")]
    InboundPeerCapacityExceeded {
        /// Peer associated with the inbound connection, when its DID parsed successfully.
        peer: Option<crate::dht::Did>,
        /// Maximum queued and executing messages retained for one peer.
        capacity: usize,
    },

    /// One peer has exhausted its inbound retained-memory allowance.
    #[error(
        "Inbound peer {peer:?} message of {requested_bytes} bytes exceeds its {capacity_bytes}-byte budget"
    )]
    InboundPeerMemoryCapacityExceeded {
        /// Peer associated with the inbound connection, when its DID parsed successfully.
        peer: Option<crate::dht::Did>,
        /// Bytes requested by the inbound message.
        requested_bytes: usize,
        /// Retained byte capacity available to one peer.
        capacity_bytes: usize,
    },

    /// The connection's inbound mailbox actor is unavailable.
    #[error("Inbound mailbox is closed")]
    InboundMailboxClosed,

    /// No Tokio runtime is available to host a native inbound actor.
    #[error("Inbound mailbox requires an active Tokio runtime")]
    InboundMailboxRuntimeUnavailable,

    /// The inbound actor observed an impossible message/lane state.
    #[error("Inbound actor state invariant violated")]
    InboundActorInvariantViolation,

    /// A reassembled chunk payload attempted to contain another chunk envelope.
    #[error("Nested chunk messages are not allowed")]
    NestedChunkMessage,

    /// A chunk was rejected for an invalid remote wire shape or metadata.
    #[error("Invalid chunk message")]
    InvalidChunkMessage,

    /// The application rejected an inbound message during validation.
    #[error("Inbound message validation failed: {source}")]
    InboundValidationFailed {
        /// Original application validation error.
        #[source]
        source: CallbackError,
    },

    /// An application validation callback did not complete within its deadline.
    #[error("Inbound validation for {peer:?} timed out after {timeout_ms}ms")]
    InboundValidationTimeout {
        /// Peer associated with the inbound connection, when its DID parsed successfully.
        peer: Option<crate::dht::Did>,
        /// Callback deadline in milliseconds.
        timeout_ms: u128,
    },

    /// An application callback failed after core inbound handling.
    #[error("Inbound message callback failed: {source}")]
    InboundCallbackFailed {
        /// Original application callback error.
        #[source]
        source: CallbackError,
    },

    /// Inbound handling and its application callback did not complete within the deadline.
    #[error("Inbound processing for {peer:?} timed out after {timeout_ms}ms")]
    InboundProcessingTimeout {
        /// Peer associated with the inbound connection, when its DID parsed successfully.
        peer: Option<crate::dht::Did>,
        /// Processing deadline in milliseconds.
        timeout_ms: u128,
    },

    /// The browser runtime could not schedule an inbound deadline timer.
    #[error("Inbound {operation} timer unavailable for {peer:?}")]
    InboundTimerUnavailable {
        /// Peer associated with the inbound connection, when its DID parsed successfully.
        peer: Option<crate::dht::Did>,
        /// Inbound phase whose deadline could not be scheduled.
        operation: &'static str,
    },

    /// Recv message through channel failed {0}
    #[error("Recv message through channel failed {0}")]
    ChannelRecvMessageFailed(String),

    /// Invalid PeerRingAction
    #[error("Invalid PeerRingAction")]
    PeerRingInvalidAction,

    /// Failed on read successors
    #[error("Failed on read successors")]
    FailedToReadSuccessors,

    /// Successor index {index} is out of bounds for length {len}
    #[error("Successor index {index} is out of bounds for length {len}")]
    SuccessorIndexOutOfBounds {
        /// Requested successor index.
        index: usize,
        /// Current successor sequence length.
        len: usize,
    },

    /// Failed on write successors
    #[error("Failed on write successors")]
    FailedToWriteSuccessors,

    /// Unexpected PeerRingAction, {0:?}
    #[error("Unexpected PeerRingAction, {0:?}")]
    PeerRingUnexpectedAction(Box<crate::dht::PeerRingAction>),

    /// Cannot seek did in swarm table, {0}
    #[error("Cannot seek did in swarm table, {0}")]
    SwarmMissDidInTable(crate::dht::Did),

    /// Cannot get transport from did: {0}
    #[error("Cannot get transport from did: {0}")]
    SwarmMissTransport(crate::dht::Did),

    /// The observed WebRTC/data-channel product state cannot make progress.
    #[error("Transport not ready: state {state:?}, data channel open: {data_channel_open}")]
    TransportNotReady {
        /// Observed WebRTC peer-connection state.
        state: rings_transport::core::transport::WebrtcConnectionState,
        /// Whether every transport data channel reported open.
        data_channel_open: bool,
    },

    /// Connection not Found
    #[error("Connection not Found")]
    ConnectionNotFound,

    /// Current node is not the next hop of message
    #[error("Current node is not the next hop of message")]
    InvalidNextHop,

    /// The payload has taken every forward its relay carrier was given
    #[error("Relay hop budget exhausted: the payload has taken every forward it was given")]
    RelayHopBudgetExhausted,

    /// A relay carrier claimed more forwards than any fresh carrier holds
    #[error("Relay hop budget {0} is above the maximum a carrier can hold")]
    RelayHopBudgetAboveMax(u8),

    /// Cannot get next hop when sending message
    #[error("Cannot get next hop when sending message")]
    NoNextHop,

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// IndexedDB error, {0}
    #[error("IndexedDB error, {0}")]
    IDBError(rexie::Error),

    /// Invalid capacity value
    #[error("Invalid capacity value")]
    InvalidCapacity,

    /// A value of {required} bytes cannot fit a storage budget of {capacity} bytes
    #[error("A value of {required} bytes cannot fit a storage budget of {capacity} bytes")]
    StorageValueExceedsCapacity {
        /// Bytes the value occupies on disk.
        required: u64,
        /// Total byte budget of the storage.
        capacity: u64,
    },

    /// Message invalid: {0}
    #[error("Message invalid: {0}")]
    InvalidMessage(String),

    /// Message encryption failed
    #[error("Message encryption failed")]
    MessageEncryptionFailed(String),

    /// Message decryption failed
    #[error("Message decryption failed")]
    MessageDecryptionFailed(String),

    /// An ElGamal AEAD envelope carries the wrong number of wrapped-key blocks.
    #[error("ElGamal AEAD wrapped-key block count mismatch: expected {expected}, actual {actual}")]
    AeadWrappedKeyBlockCount {
        /// Number of blocks required to encode the fixed-size AEAD key.
        expected: usize,
        /// Number of blocks supplied by the envelope.
        actual: usize,
    },

    /// Message has {0} bytes which is too large
    #[error("Message has {0} bytes which is too large")]
    MessageTooLarge(usize),

    /// A serialized message size cannot be represented by the local platform.
    #[error("Serialized message size exceeds the local platform limit")]
    MessageSizeOverflow,

    /// Peer's negotiated max_message_size {0} is too small to carry even one chunk
    #[error("Peer's negotiated max_message_size {0} is too small to carry even one chunk")]
    PeerMaxMessageSizeTooSmall(usize),

    /// Timed out while waiting for the data-channel send queue to accept bytes
    #[error(
        "Timed out after {timeout_ms}ms waiting for data-channel send queue to accept {bytes} bytes for {peer} during {context}"
    )]
    DataChannelSendQueueTimeout {
        /// Peer whose data-channel send queue did not accept the bytes.
        peer: crate::dht::Did,
        /// Timeout budget in milliseconds.
        timeout_ms: u128,
        /// Serialized bytes that were waiting to be accepted.
        bytes: usize,
        /// Send context used for diagnostics.
        context: &'static str,
    },
    /// Timed out after the backend crossed its final cancellable send boundary.
    #[error(
        "Timed out after {timeout_ms}ms completing an irrevocable {bytes}-byte data-channel send to {peer} during {context}"
    )]
    DataChannelSendCompletionTimeout {
        /// Peer whose irrevocable backend send did not complete.
        peer: crate::dht::Did,
        /// Completion timeout budget in milliseconds.
        timeout_ms: u128,
        /// Number of bytes owned by the backend send.
        bytes: usize,
        /// Scheduler phase that issued the send.
        context: &'static str,
    },

    /// Timed out while waiting for accepted data-channel bytes to leave the local buffer.
    #[error(
        "Timed out after {timeout_ms}ms waiting for data-channel delivery to {peer} during {context}"
    )]
    DataChannelDeliveryTimeout {
        /// Peer whose accepted bytes did not leave the local send buffer.
        peer: crate::dht::Did,
        /// Delivery timeout budget in milliseconds.
        timeout_ms: u128,
        /// Send context used for diagnostics.
        context: &'static str,
    },

    /// A tracked transfer did not stop within its post-deadline cleanup grace.
    #[error(
        "Tracked payload cleanup for {peer} exceeded its {timeout_ms}ms grace after the send deadline"
    )]
    TrackedPayloadCleanupTimeout {
        /// Peer whose exact connection generation was made send-terminal.
        peer: crate::dht::Did,
        /// Cleanup grace in milliseconds.
        timeout_ms: u128,
    },

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// Error on ser/der JsValue
    #[error("Error on ser/der JsValue")]
    SerdeWasmBindgenError(#[from] serde_wasm_bindgen::Error),

    /// Delegation is expired
    #[error("Delegation is expired")]
    DelegationExpired,

    /// Transport error: {0}
    #[error("Transport error: {0}")]
    Transport(#[from] rings_transport::error::Error),

    /// External Javascript error: {0}
    #[error("External Javascript error: {0}")]
    JsError(String),
}
