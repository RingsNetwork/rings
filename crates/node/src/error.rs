//! A bunch of wrap errors.
use rings_core::dht::Did;

use crate::onion::OnionProxyTargetError;
use crate::onion::OnionRouteError;
use crate::prelude::rings_core;

/// Bounded onion/relay queue whose admission failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionQueueKind {
    /// Terminal relay-control sends.
    RelayControl,
    /// Onion data-plane sends.
    CircuitData,
}

impl OnionQueueKind {
    /// Build the closed admission error for this queue kind.
    pub(crate) const fn admission(self, peer: Did, reason: OnionQueueAdmissionReason) -> Error {
        Error::OnionQueueAdmission {
            queue: self,
            peer,
            reason,
        }
    }
}

/// Algebraic reason a bounded onion/relay queue rejected one item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnionQueueAdmissionReason {
    /// The queue-wide bound was reached.
    GlobalFull,
    /// One peer reached its isolated share.
    PeerFull,
    /// A resource counter could not represent its successor.
    CounterOverflow,
}

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
/// The error type can be expressed in decimal, where the high decs represent
/// the error category and the low decs represent the error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[repr(u32)]
pub enum Error {
    /// Connecting to a remote JSON-RPC server failed.
    #[error("Connect remote rpc server failed: {0}.")]
    RemoteRpcError(String) = 100,
    /// A remote JSON-RPC error did not match a known local variant.
    #[error("Unknown rpc error.")]
    UnknownRpcError = 101,
    /// The internal JSON-RPC dispatcher returned an error.
    #[error("Internal rpc services error: {0}.")]
    InternalRpcError(#[from] jsonrpc_core::Error) = 102,
    /// UUID parsing or formatting failed.
    #[error("Uuid error: {0}")]
    UuidError(#[from] uuid::Error) = 103,
    /// The requested connection does not exist in the local swarm.
    #[error("Connection not found.")]
    ConnectionNotFound = 203,
    /// Opening a new connection through the core swarm failed.
    #[error("Create connection error: {0}.")]
    NewConnectionError(rings_core::error::Error) = 204,
    /// Closing an existing connection through the core swarm failed.
    #[error("Close connection error: {0}.")]
    CloseConnectionError(rings_core::error::Error) = 205,
    /// The supplied connection identifier is malformed or unknown.
    #[error("Invalid connection id.")]
    InvalidConnectionId = 206,
    /// Creating WebRTC offer data failed.
    #[error("Create offer info failed: {0}.")]
    CreateOffer(rings_core::error::Error) = 207,
    /// Creating WebRTC answer data failed.
    #[error("Answer offer info failed: {0}.")]
    AnswerOffer(rings_core::error::Error) = 208,
    /// Accepting WebRTC answer data failed.
    #[error("Accept answer info failed: {0}.")]
    AcceptAnswer(rings_core::error::Error) = 209,
    /// Decoding structured data failed.
    #[error("Decode error.")]
    DecodeError = 300,
    /// Encoding structured data failed.
    #[error("Encode error.")]
    EncodeError = 301,
    /// Compiling a WASM artifact failed.
    #[error("WASM compile error: {0}")]
    WasmCompileError(String) = 400,
    /// Acquiring the WASM backend-message lock failed.
    #[error("BackendMessage RwLock Error")]
    WasmBackendMessageRwLockError = 401,
    /// Instantiating a WASM module failed.
    #[error("WASM instantiation error.")]
    WasmInstantiationError = 402,
    /// Resolving a required WASM export failed.
    #[error("WASM export error.")]
    WasmExportError = 403,
    /// Executing WASM code returned a runtime error.
    #[error("WASM runtime error: {0}")]
    WasmRuntimeError(String) = 404,
    /// Acquiring the WASM global memory lock failed.
    #[error("WASM global memory mutex error.")]
    WasmGlobalMemoryLockError = 405,
    /// Loading a WASM input file failed.
    #[error("WASM failed to load file.")]
    WasmFailedToLoadFile = 406,
    /// Parsing or validating a DID failed.
    #[error("Invalid did: {0}")]
    InvalidDid(String) = 500,
    /// The requested JSON-RPC method is not supported.
    #[error("Invalid method.")]
    InvalidMethod = 501,
    /// A core internal operation failed.
    #[error("Internal error: {0}.")]
    InternalError(rings_core::error::Error) = 502,
    /// The caller is not authorized to perform the requested action.
    #[error("No Permission")]
    NoPermission = 504,
    /// Connecting to a peer failed.
    #[error("Connect error, {0}")]
    ConnectError(rings_core::error::Error) = 600,
    /// Sending a peer message failed.
    #[error("Send message error: {0}")]
    SendMessage(rings_core::error::Error) = 601,
    /// Applying a DHT entry action failed.
    #[error("entry action error: {0}")]
    EntryError(rings_core::error::Error) = 603,
    /// Registering a service descriptor failed.
    #[error("service register action error: {0}")]
    ServiceRegisterError(rings_core::error::Error) = 604,
    /// JavaScript host code returned an error.
    #[error("JsError: {0}")]
    JsError(String) = 700,
    /// A protocol message failed validation.
    #[error("Invalid message")]
    InvalidMessage = 800,
    /// An HTTP proxy request failed validation.
    #[error("Invalid http request: {0}")]
    HttpRequestError(String) = 801,
    /// Input data failed semantic validation.
    #[error("Invalid data")]
    InvalidData = 802,
    /// A service name or service descriptor is invalid.
    #[error("Invalid service")]
    InvalidService = 803,
    /// A network address is invalid.
    #[error("Invalid address")]
    InvalidAddress = 804,
    /// Authentication payload data is invalid.
    #[error("Invalid auth data")]
    InvalidAuthData = 805,
    /// Request headers are missing required values or contain invalid values.
    #[error("invalid headers")]
    InvalidHeaders = 806,
    /// Persistent or cache storage returned an error.
    #[error("Storage Error: {0}")]
    Storage(rings_core::error::Error) = 807,
    /// Swarm state management returned an error.
    #[error("Swarm Error: {0}")]
    Swarm(rings_core::error::Error) = 808,
    /// The requested logging level is not supported.
    #[error("Invalid logging level: {0}")]
    InvalidLoggingLevel(String) = 809,
    /// The configured WebRTC UDP port range is invalid.
    #[error("Invalid WebRTC UDP port range: {0}")]
    InvalidWebrtcUdpPortRange(#[from] rings_transport::webrtc_config::WebrtcUdpPortRangeError) =
        810,
    /// Only one side of the WebRTC UDP port range was configured.
    #[error("Both webrtc_udp_port_min and webrtc_udp_port_max must be set together: min={min:?}, max={max:?}")]
    IncompleteWebrtcUdpPortRange {
        /// Configured lower UDP port bound.
        min: Option<u16>,
        /// Configured upper UDP port bound.
        max: Option<u16>,
    } = 811,
    /// The node configuration is structurally invalid.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String) = 812,
    /// Opening browser IndexedDB-backed provider storage failed.
    #[error("Open browser storage \"{name}\" failed: {source}")]
    BrowserStorageOpen {
        /// IndexedDB database and object-store name requested by the browser provider.
        name: String,
        /// Storage backend error returned while opening the database.
        source: rings_core::error::Error,
    } = 814,
    /// A periodic registration task observed a cooperative stop request.
    #[error("registration task stopped")]
    RegistrationStopped = 815,
    /// Loading or explicitly flushing local peer measurements failed.
    #[error("Measurement runtime error: {0}")]
    MeasurementRuntime(#[from] crate::measure::MeasureRuntimeError) = 816,
    /// Creating a file on disk failed.
    #[error("Create File Error: {0}")]
    CreateFileError(String) = 900,
    /// Opening a file on disk failed.
    #[error("Open File Error: {0}")]
    OpenFileError(String) = 901,
    /// Acquiring a synchronization lock failed.
    #[error("Acquire lock failed")]
    Lock = 902,
    /// The process home directory could not be resolved.
    #[error("Cannot find home directory")]
    HomeDirError = 903,
    /// The parent directory of a path could not be resolved.
    #[error("Cannot find parent directory")]
    ParentDirError = 904,
    /// A filesystem path cannot be represented as UTF-8.
    #[error("Path is not valid UTF-8: {0}")]
    PathUtf8Error(String) = 905,
    /// JSON serialization or deserialization failed.
    #[error("Serde json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error) = 1000,
    /// YAML serialization or deserialization failed.
    #[error("Serde yaml error: {0}")]
    SerdeYamlError(#[from] serde_yaml::Error) = 1001,
    /// Cryptographic verification failed.
    #[error("verify error: {0}")]
    VerifyError(String) = 1002,
    /// A rings-core operation returned an error.
    #[error("Core error: {0}")]
    CoreError(#[from] rings_core::error::Error) = 1102,
    /// An external signer returned an error.
    #[error("External singer error: {0}")]
    ExternalError(String) = 1202,
    /// An FFI string contained an interior nul byte.
    #[error("An error indicating that an interior nul byte was found: {0}")]
    FFINulError(#[from] std::ffi::NulError) = 1203,
    /// Converting an FFI C string to UTF-8 failed.
    #[error("Failed to convert CStr to String: {0}")]
    FFICStrError(#[from] std::str::Utf8Error) = 1204,
    /// An FFI pointer argument was null.
    #[error("An error indicating that a ptr is null")]
    FFINulPtrError = 1205,
    /// Converting owned FFI bytes to UTF-8 failed.
    #[error("Failed to convert bytes to String: {0}")]
    FFIFromUtf8Error(#[from] std::string::FromUtf8Error) = 1206,
    /// A protocol backend returned an error.
    #[error("Extend Backend Error {0}")]
    BackendError(String) = 1501,
    /// An extension runtime returned an error.
    #[error("Extension error: {0}")]
    ExtensionError(String) = 1502,
    /// An owned extension task ended before publishing its result.
    #[error("Detached extension task closed before publishing its result")]
    DetachedExtensionTaskClosed = 1503,
    /// Onion route construction or validation failed.
    #[error("Onion route error: {0}")]
    OnionRouteError(OnionRouteError) = 1601,
    /// Onion proxy I/O failed.
    #[error("Onion proxy IO error: {0}")]
    OnionProxyIoError(String) = 1602,
    /// A local onion proxy request did not complete before its deadline.
    #[error("Onion proxy request timed out")]
    OnionProxyRequestTimedOut = 1603,
    /// A bounded onion or relay send queue rejected one item.
    #[error("{queue:?} queue rejected peer {peer}: {reason:?}")]
    OnionQueueAdmission {
        /// Queue whose bound was reached.
        queue: OnionQueueKind,
        /// Peer whose item was rejected.
        peer: Did,
        /// Exact admission relation that failed.
        reason: OnionQueueAdmissionReason,
    } = 1604,
    /// An onion proxy authority failed closed parsing.
    #[error("Invalid onion proxy target: {0}")]
    OnionProxyTarget(#[from] OnionProxyTargetError) = 1605,
    /// Runtime DNS resolution of an admitted onion target failed.
    #[error("Failed to resolve onion target {authority:?}: {source}")]
    OnionTargetResolve {
        /// Canonical target authority.
        authority: String,
        /// Resolver I/O failure.
        source: std::io::Error,
    } = 1606,
    /// Runtime DNS resolution returned no addresses.
    #[error("Onion target {authority:?} resolved no addresses")]
    OnionTargetResolvedEmpty {
        /// Canonical target authority.
        authority: String,
    } = 1607,
}

impl Error {
    fn discriminant(&self) -> u32 {
        // SAFETY: Because `Self` is marked `repr(u32)`, its layout is a `repr(C)` `union`
        // between `repr(C)` structs, each of which has the `u32` discriminant as its first
        // field, so we can read the discriminant without offsetting the pointer.
        // This code is copy from
        // ref: https://doc.rust-lang.org/std/mem/fn.discriminant.html
        // And we modify it from [u8] to [u32], this is work because
        // repr(C) is equivalent to one of repr(u*) (see the next section) for
        // fieldless enums.
        // ref: https://doc.rust-lang.org/nomicon/other-reprs.html
        unsafe { *<*const _>::from(self).cast::<u32>() }
    }

    /// Returns the stable numeric error code for JSON-RPC error conversion.
    pub fn code(&self) -> u32 {
        self.discriminant()
    }
}

impl From<Error> for jsonrpc_core::Error {
    fn from(e: Error) -> Self {
        Self {
            code: jsonrpc_core::ErrorCode::ServerError(e.code().into()),
            message: e.to_string(),
            data: None,
        }
    }
}

impl From<rings_rpc::error::Error> for Error {
    fn from(e: rings_rpc::error::Error) -> Self {
        match e {
            rings_rpc::error::Error::InvalidMethod => Error::InvalidMethod,
            rings_rpc::error::Error::RpcError(v) => Error::RemoteRpcError(v.to_string()),
            rings_rpc::error::Error::InvalidSignature => Error::InvalidData,
            rings_rpc::error::Error::InvalidHeaders => Error::InvalidHeaders,
            _ => Error::UnknownRpcError,
        }
    }
}

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl From<Error> for wasm_bindgen::JsValue {
    fn from(err: Error) -> Self {
        wasm_bindgen::JsValue::from_str(&err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_error_code() {
        let err = Error::RemoteRpcError("Test".to_string());
        assert_eq!(err.code(), 100);
    }
}
