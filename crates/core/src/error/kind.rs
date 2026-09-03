use super::CallbackError;

/// Errors collections in ring-core.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
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

    /// Failed to decode vector to bls affine
    #[error("Failed to decode vector to bls affine")]
    BlsAffineDecodeFailed,

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

    /// Couldn't decode data as UTF-8.
    #[error("Couldn't decode data as UTF-8.")]
    Utf8Encoding(#[from] std::string::FromUtf8Error),

    /// IOError
    #[error("IOError")]
    ServiceIOError(#[from] std::io::Error),

    /// Invalid hexadecimal id in directory cache
    #[error("Invalid hexadecimal id in directory cache")]
    BadHexInCache(#[from] hex::FromHexError),

    /// Invalid rustc hexadecimal id in directory cache
    #[error("Invalid rustc hexadecimal id in directory cache")]
    BadCHexInCache,

    /// URL parse error
    #[error("URL parse error")]
    URLParse(#[from] url::ParseError),

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

    /// Gzip encode error.
    #[error("Gzip encode error.")]
    GzipEncode,

    /// Gzip decode error.
    #[error("Gzip decode error.")]
    GzipDecode,

    /// Failed on promise, state is not succeeded
    #[error("Failed on promise, state is not succeeded")]
    PromiseStateFailed,

    /// promise timeout, state is not succeeded
    #[error("promise timeout, state is not succeeded")]
    PromiseStateTimeout,

    /// Ice server scheme {0} has not supported yet
    #[error("Ice server scheme {0} has not supported yet")]
    IceServerSchemeNotSupport(String),

    /// Ice server get url without host
    #[error("Ice server get url without host")]
    IceServerURLMissHost,

    /// Cannot find next node by local DHT
    #[error("Cannot find next node by local DHT")]
    MessageHandlerMissNextNode,

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

    /// Failed on TryInto Entry
    #[error("Failed on TryInto Entry")]
    PeerRingInvalidEntry,

    /// Unexpected PeerRingAction, {0:?}
    #[error("Unexpected PeerRingAction, {0:?}")]
    PeerRingUnexpectedAction(Box<crate::dht::PeerRingAction>),

    /// PeerRing findsuccessor error, {0}
    #[error("PeerRing findsuccessor error, {0}")]
    PeerRingFindSuccessor(String),

    /// PeerRing cannot find closest preceding node
    #[error("PeerRing cannot find closest preceding node")]
    PeerRingNotFindClosestNode,

    /// PeerRing RWLock unlock failed
    #[error("PeerRing RWLock unlock failed")]
    PeerRingUnlockFailed,

    /// Cannot seek did in swarm table, {0}
    #[error("Cannot seek did in swarm table, {0}")]
    SwarmMissDidInTable(crate::dht::Did),

    /// Cannot gather local candidate, {0}
    #[error("Cannot gather local candidate, {0}")]
    FailedOnGatherLocalCandidate(String),

    /// Node behaviour bad
    #[error("Node behaviour bad")]
    NodeBehaviourBad(crate::dht::Did),

    /// Cannot get transport from did: {0}
    #[error("Cannot get transport from did: {0}")]
    SwarmMissTransport(crate::dht::Did),

    /// Load message failed with message: {0}
    #[error("Load message failed with message: {0}")]
    SwarmLoadMessageRecvFailed(String),

    /// Default transport is not connected
    #[error("Default transport is not connected")]
    SwarmDefaultTransportNotConnected,

    /// call lock() failed
    #[error("call lock() failed")]
    SwarmPendingTransTryLockFailed,

    /// transport not found
    #[error("transport not found")]
    SwarmPendingTransNotFound,

    /// failed to close previous when registering, {0}
    #[error("failed to close previous when registering, {0}")]
    SwarmToClosePrevTransport(String),

    /// call lock() failed
    #[error("call lock() failed")]
    SessionTryLockFailed,

    /// Invalid peer type
    #[error("Invalid peer type")]
    InvalidPeerType,

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC new peer connection failed
    #[error("RTC new peer connection failed")]
    RTCPeerConnectionCreateFailed(#[source] webrtc::Error),

    /// RTC peer_connection not establish
    #[error("RTC peer_connection not establish")]
    RTCPeerConnectionNotEstablish,

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection fail to create offer
    #[error("RTC peer_connection fail to create offer")]
    RTCPeerConnectionCreateOfferFailed(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// RTC peer_connection fail to create offer
    #[error("RTC peer_connection fail to create offer")]
    RTCPeerConnectionCreateOfferFailed(String),

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection fail to create answer
    #[error("RTC peer_connection fail to create answer")]
    RTCPeerConnectionCreateAnswerFailed(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// RTC peer_connection fail to create answer
    #[error("RTC peer_connection fail to create answer")]
    RTCPeerConnectionCreateAnswerFailed(String),

    /// DataChannel message size not match, {0} < {1}
    #[error("DataChannel message size not match, {0} < {1}")]
    RTCDataChannelMessageIncomplete(usize, usize),

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// DataChannel send text message failed
    #[error("DataChannel send text message failed")]
    RTCDataChannelSendTextFailed(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// DataChannel send text message failed, {0}
    #[error("DataChannel send text message failed, {0}")]
    RTCDataChannelSendTextFailed(String),

    /// DataChannel not ready
    #[error("DataChannel not ready")]
    RTCDataChannelNotReady,

    /// DataChannel state not open
    #[error("DataChannel state not open")]
    RTCDataChannelStateNotOpen,

    /// The observed WebRTC/data-channel product state cannot make progress.
    #[error("Transport not ready: state {state:?}, data channel open: {data_channel_open}")]
    TransportNotReady {
        /// Observed WebRTC peer-connection state.
        state: rings_transport::core::transport::WebrtcConnectionState,
        /// Whether every transport data channel reported open.
        data_channel_open: bool,
    },

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection add ice candidate error
    #[error("RTC peer_connection add ice candidate error")]
    RTCPeerConnectionAddIceCandidateError(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// RTC peer_connection add ice candidate error
    #[error("RTC peer_connection add ice candidate error")]
    RTCPeerConnectionAddIceCandidateError(String),

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection set local description failed
    #[error("RTC peer_connection set local description failed")]
    RTCPeerConnectionSetLocalDescFailed(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// RTC peer_connection set local description failed
    #[error("RTC peer_connection set local description failed")]
    RTCPeerConnectionSetLocalDescFailed(String),

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection set remote description failed
    #[error("RTC peer_connection set remote description failed")]
    RTCPeerConnectionSetRemoteDescFailed(#[source] webrtc::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// RTC peer_connection set remote description failed
    #[error("RTC peer_connection set remote description failed")]
    RTCPeerConnectionSetRemoteDescFailed(String),

    #[cfg(not(all(feature = "wasm", target_family = "wasm")))]
    /// RTC peer_connection failed to close it
    #[error("RTC peer_connection failed to close it")]
    RTCPeerConnectionCloseFailed(#[source] webrtc::Error),

    /// RTC unsupported sdp type
    #[error("RTC unsupported sdp type")]
    RTCSdpTypeNotMatch,

    /// Connection not Found
    #[error("Connection not Found")]
    ConnectionNotFound,

    /// Invalid Transport Id
    #[error("Invalid Transport Id")]
    InvalidTransportUuid,

    /// Unexpected encrypted data
    #[error("Unexpected encrypted data")]
    UnexpectedEncryptedData,

    /// Failed to decrypt data
    #[error("Failed to decrypt data")]
    DecryptionError,

    /// Current node is not the next hop of message
    #[error("Current node is not the next hop of message")]
    InvalidNextHop,

    /// Adjacent elements in path cannot be equal
    #[error("Adjacent elements in path cannot be equal")]
    InvalidRelayPath,

    /// Suspected infinite looping in path
    #[error("Suspected infinite looping in path")]
    InfiniteRelayPath,

    /// The destination of report message should always be the first element of path
    #[error("The destination of report message should always be the first element of path")]
    InvalidRelayDestination,

    /// Cannot infer next hop
    #[error("Cannot infer next hop")]
    CannotInferNextHop,

    /// Cannot get next hop when sending message
    #[error("Cannot get next hop when sending message")]
    NoNextHop,

    /// To generate REPORT, you should provide SEND
    #[error("To generate REPORT, you should provide SEND")]
    ReportNeedSend,

    /// Only SEND message can reset destination
    #[error("Only SEND message can reset destination")]
    ResetDestinationNeedSend,

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

    /// entry not found
    #[error("entry not found")]
    EntryNotFound,

    /// IO error: {0}
    #[error("IO error: {0}")]
    IOError(std::io::Error),

    /// Failed to get dht from a sync lock
    #[error("Failed to get dht from a sync lock")]
    DHTSyncLockError,

    /// Failed to lock callback of swarm
    #[error("Failed to lock callback of swarm")]
    CallbackSyncLockError,

    /// Failed to build swarm: {0}
    #[error("Failed to build swarm: {0}")]
    SwarmBuildFailed(String),

    /// Message invalid: {0}
    #[error("Message invalid: {0}")]
    InvalidMessage(String),

    /// Message encryption failed
    #[error("Message encryption failed")]
    MessageEncryptionFailed(String),

    /// Message decryption failed
    #[error("Message decryption failed")]
    MessageDecryptionFailed(String),

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
    /// Cannot get property {0} from JsValue
    #[error("Cannot get property {0} from JsValue")]
    FailedOnGetProperty(String),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// Cannot set property {0} from JsValue
    #[error("Cannot set property {0} from JsValue")]
    FailedOnSetProperty(String),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// Error on ser/der JsValue
    #[error("Error on ser/der JsValue")]
    SerdeWasmBindgenError(#[from] serde_wasm_bindgen::Error),

    #[cfg(all(feature = "wasm", target_family = "wasm"))]
    /// Error create RTC connection: {0}
    #[error("Error create RTC connection: {0}")]
    CreateConnectionError(String),

    /// Session is expired
    #[error("Session is expired")]
    SessionExpired,

    /// Transport error: {0}
    #[error("Transport error: {0}")]
    Transport(#[from] rings_transport::error::Error),

    /// External Javascript error: {0}
    #[error("External Javascript error: {0}")]
    JsError(String),
}
