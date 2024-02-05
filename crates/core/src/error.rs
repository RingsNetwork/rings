//! Error of rings_core

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors collections in ring-core.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("Ed25519/EdDSA pubkey bad format")]
    EdDSAPublicKeyBadFormat,

    #[error("Secp256k1/ECDSA pubkey bad format")]
    ECDSAPublicKeyBadFormat,

    #[error("Secp256r1/ECDSA Error: {0}")]
    ECDSAError(#[from] ecdsa::Error),

    #[error("ECDSA or EdDSA pubkey bad format")]
    PublicKeyBadFormat,

    #[error("Invalid Transport")]
    InvalidTransport,

    #[error("InvalidPublicKey")]
    InvalidPublicKey,

    #[error("VirtualNode kind not equal when overwriting")]
    VNodeKindNotEqual,

    #[error("Did of VirtualNode not equal")]
    VNodeDidNotEqual,

    #[error("The type of VirtualNode is not allowed to be overwritten")]
    VNodeNotOverwritable,

    #[error("The type of VirtualNode is not allowed to be appended")]
    VNodeNotAppendable,

    #[error("The type of VirtualNode is not allowed to be joined as a subring")]
    VNodeNotJoinable,

    #[error("Encode a byte vector into a base58-check string, adds 4 bytes checksum")]
    Encode,

    #[error("Decode base58-encoded with 4 bytes checksum string into a byte vector")]
    Decode,

    #[error("Couldn't decode data as UTF-8.")]
    Utf8Encoding(#[from] std::string::FromUtf8Error),

    #[error("IOError")]
    ServiceIOError(#[from] std::io::Error),

    #[error("Invalid hexadecimal id in directory cache")]
    BadHexInCache(#[from] hex::FromHexError),

    #[error("Invalid rustc hexadecimal id in directory cache")]
    BadCHexInCache,

    #[error("URL parse error")]
    URLParse(#[from] url::ParseError),

    #[error("Invalid hexadecimal id in directory cache")]
    BadArrayInCache(#[from] std::array::TryFromSliceError),

    #[error("JSON serialize toString error")]
    SerializeToString,

    #[error("Serialization error")]
    SerializeError,

    #[error("JSON serialization error")]
    Serialize(#[source] serde_json::Error),

    #[error("JSON deserialization error")]
    Deserialize(#[source] serde_json::Error),

    #[error("Bincode serialization error")]
    BincodeSerialize(#[source] bincode::Error),

    #[error("Bincode deserialization error")]
    BincodeDeserialize(#[source] bincode::Error),

    #[error("Unknown account")]
    UnknownAccount,

    #[error("Failed on verify message signature")]
    VerifySignatureFailed,

    #[error("Gzip encode error.")]
    GzipEncode,

    #[error("Gzip decode error.")]
    GzipDecode,

    #[error("Failed on promise, state is not succeeded")]
    PromiseStateFailed,

    #[error("promise timeout, state is not succeeded")]
    PromiseStateTimeout,

    #[error("Ice server scheme {0} has not supported yet")]
    IceServerSchemeNotSupport(String),

    #[error("Ice server get url without host")]
    IceServerURLMissHost,

    #[error("SecretKey parse error, {0}")]
    Libsecp256k1SecretKeyParse(String),

    #[error("Signature standard parse failed, {0}")]
    Libsecp256k1SignatureParseStandard(String),

    #[error("RecoverId parse failed, {0}")]
    Libsecp256k1RecoverIdParse(String),

    #[error("Libsecp256k1 recover failed")]
    Libsecp256k1Recover,

    #[error("Cannot find next node by local DHT")]
    MessageHandlerMissNextNode,

    #[error("Found existing transport when answer offer from remote node")]
    AlreadyConnected,

    #[error("You should not connect to yourself")]
    ShouldNotConnectSelf,

    #[error("Send message through channel failed")]
    ChannelSendMessageFailed,

    #[error("Recv message through channel failed {0}")]
    ChannelRecvMessageFailed(String),

    #[error("Invalid PeerRingAction")]
    PeerRingInvalidAction,

    #[error("Failed on read successors")]
    FailedToReadSuccessors,

    #[error("Failed on write successors")]
    FailedToWriteSuccessors,

    #[error("Failed on TryInto VNode")]
    PeerRingInvalidVNode,

    #[error("Unexpected PeerRingAction, {0:?}")]
    PeerRingUnexpectedAction(crate::dht::PeerRingAction),

    #[error("PeerRing findsuccessor error, {0}")]
    PeerRingFindSuccessor(String),

    #[error("PeerRing cannot find closest preceding node")]
    PeerRingNotFindClosestNode,

    #[error("PeerRing RWLock unlock failed")]
    PeerRingUnlockFailed,

    #[error("Cannot seek did in swarm table, {0}")]
    SwarmMissDidInTable(crate::dht::Did),

    #[error("Cannot gather local candidate, {0}")]
    FailedOnGatherLocalCandidate(String),

    #[error("Node behaviour bad")]
    NodeBehaviourBad(crate::dht::Did),

    #[error("Cannot get transport from did: {0}")]
    SwarmMissTransport(crate::dht::Did),

    #[error("Load message failed with message: {0}")]
    SwarmLoadMessageRecvFailed(String),

    #[error("Default transport is not connected")]
    SwarmDefaultTransportNotConnected,

    #[error("call lock() failed")]
    SwarmPendingTransTryLockFailed,

    #[error("transport not found")]
    SwarmPendingTransNotFound,

    #[error("failed to close previous when registering, {0}")]
    SwarmToClosePrevTransport(String),

    #[error("call lock() failed")]
    SessionTryLockFailed,

    #[error("Invalid peer type")]
    InvalidPeerType,

    #[error("Invalid virtual node type")]
    InvalidVNodeType,

    #[cfg(not(feature = "wasm"))]
    #[error("RTC new peer connection failed")]
    RTCPeerConnectionCreateFailed(#[source] webrtc::Error),

    #[error("RTC peer_connection not establish")]
    RTCPeerConnectionNotEstablish,

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection fail to create offer")]
    RTCPeerConnectionCreateOfferFailed(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("RTC peer_connection fail to create offer")]
    RTCPeerConnectionCreateOfferFailed(String),

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection fail to create answer")]
    RTCPeerConnectionCreateAnswerFailed(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("RTC peer_connection fail to create answer")]
    RTCPeerConnectionCreateAnswerFailed(String),

    #[error("DataChannel message size not match, {0} < {1}")]
    RTCDataChannelMessageIncomplete(usize, usize),

    #[cfg(not(feature = "wasm"))]
    #[error("DataChannel send text message failed")]
    RTCDataChannelSendTextFailed(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("DataChannel send text message failed, {0}")]
    RTCDataChannelSendTextFailed(String),

    #[error("DataChannel not ready")]
    RTCDataChannelNotReady,

    #[error("DataChannel state not open")]
    RTCDataChannelStateNotOpen,

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection add ice candidate error")]
    RTCPeerConnectionAddIceCandidateError(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("RTC peer_connection add ice candidate error")]
    RTCPeerConnectionAddIceCandidateError(String),

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection set local description failed")]
    RTCPeerConnectionSetLocalDescFailed(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("RTC peer_connection set local description failed")]
    RTCPeerConnectionSetLocalDescFailed(String),

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection set remote description failed")]
    RTCPeerConnectionSetRemoteDescFailed(#[source] webrtc::Error),

    #[cfg(feature = "wasm")]
    #[error("RTC peer_connection set remote description failed")]
    RTCPeerConnectionSetRemoteDescFailed(String),

    #[cfg(not(feature = "wasm"))]
    #[error("RTC peer_connection failed to close it")]
    RTCPeerConnectionCloseFailed(#[source] webrtc::Error),

    #[error("RTC unsupported sdp type")]
    RTCSdpTypeNotMatch,

    #[error("Connection not Found")]
    ConnectionNotFound,

    #[error("Invalid Transport Id")]
    InvalidTransportUuid,

    #[error("Unexpected encrypted data")]
    UnexpectedEncryptedData,

    #[error("Failed to decrypt data")]
    DecryptionError,

    #[error("Current node is not the next hop of message")]
    InvalidNextHop,

    #[error("Adjacent elements in path cannot be equal")]
    InvalidRelayPath,

    #[error("Suspected infinite looping in path")]
    InfiniteRelayPath,

    #[error("The destination of report message should always be the first element of path")]
    InvalidRelayDestination,

    #[error("Cannot infer next hop")]
    CannotInferNextHop,

    #[error("Cannot get next hop when sending message")]
    NoNextHop,

    #[error("To generate REPORT, you should provide SEND")]
    ReportNeedSend,

    #[error("Only SEND message can reset destination")]
    ResetDestinationNeedSend,

    #[cfg(feature = "wasm")]
    #[error("IndexedDB error, {0}")]
    IDBError(rexie::Error),

    #[error("Invalid capacity value")]
    InvalidCapacity,

    #[cfg(not(feature = "wasm"))]
    #[error("Sled error, {0}")]
    SledError(sled::Error),

    #[error("entry not found")]
    EntryNotFound,

    #[error("IO error: {0}")]
    IOError(std::io::Error),

    #[error("Failed to get dht from a sync lock")]
    DHTSyncLockError,

    #[error("Failed to lock callback of swarm")]
    CallbackSyncLockError,

    #[error("Failed to build swarm: {0}")]
    SwarmBuildFailed(String),

    #[error("Message invalid: {0}")]
    InvalidMessage(String),

    #[error("Message encryption failed")]
    MessageEncryptionFailed(ecies::SecpError),

    #[error("Message decryption failed")]
    MessageDecryptionFailed(ecies::SecpError),

    #[error("Message has {0} bytes which is too large")]
    MessageTooLarge(usize),

    #[cfg(feature = "wasm")]
    #[error("Cannot get property {0} from JsValue")]
    FailedOnGetProperty(String),

    #[cfg(feature = "wasm")]
    #[error("Cannot set property {0} from JsValue")]
    FailedOnSetProperty(String),

    #[cfg(feature = "wasm")]
    #[error("Error on ser/der JsValue")]
    SerdeWasmBindgenError(#[from] serde_wasm_bindgen::Error),

    #[cfg(feature = "wasm")]
    #[error("Error create RTC connection: {0}")]
    CreateConnectionError(String),

    #[error("Session is expired")]
    SessionExpired,

    #[error("Transport error: {0}")]
    Transport(#[from] rings_transport::error::Error),

    #[error("External Javascript error: {0}")]
    JsError(String),
}

#[cfg(feature = "wasm")]
impl From<Error> for wasm_bindgen::JsValue {
    fn from(err: Error) -> Self {
        wasm_bindgen::JsValue::from_str(&err.to_string())
    }
}

#[cfg(feature = "wasm")]
impl From<js_sys::Error> for Error {
    fn from(err: js_sys::Error) -> Self {
        Error::JsError(err.to_string().into())
    }
}
