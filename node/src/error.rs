//! A bunch of wrap errors.
use crate::prelude::jsonrpc_core;
use crate::prelude::rings_core;

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
/// The error type can be expressed in decimal, where the high decs represent
/// the error category and the low decs represent the error type.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[repr(u32)]
pub enum Error {
    #[error("Connect remote rpc server failed: {0}.")]
    RemoteRpcError(String) = 100,
    #[error("Unknown rpc error.")]
    UnknownRpcError = 101,
    #[error("Internal rpc services error: {0}.")]
    InternalRpcError(#[from] jsonrpc_core::Error) = 102,
    #[error("Connection not found.")]
    ConnectionNotFound = 203,
    #[error("Create connection error: {0}.")]
    NewConnectionError(rings_core::error::Error) = 204,
    #[error("Close connection error: {0}.")]
    CloseConnectionError(rings_core::error::Error) = 205,
    #[error("Invalid connection id.")]
    InvalidConnectionId = 206,
    #[error("Create offer info failed: {0}.")]
    CreateOffer(rings_core::error::Error) = 207,
    #[error("Answer offer info failed: {0}.")]
    AnswerOffer(rings_core::error::Error) = 208,
    #[error("Accept answer info failed: {0}.")]
    AcceptAnswer(rings_core::error::Error) = 209,
    #[error("Decode error.")]
    DecodeError = 300,
    #[error("Encode error.")]
    EncodeError = 301,
    #[error("WASM compile error: {0}")]
    WasmCompileError(String) = 400,
    #[error("BackendMessage RwLock Error")]
    WasmBackendMessageRwLockError = 401,
    #[error("WASM instantiation error.")]
    WasmInstantiationError = 402,
    #[error("WASM export error.")]
    WasmExportError = 403,
    #[error("WASM runtime error: {0}")]
    WasmRuntimeError(String) = 404,
    #[error("WASM global memory mutex error.")]
    WasmGlobalMemoryLockError = 405,
    #[error("WASM failed to load file.")]
    WasmFailedToLoadFile = 406,
    #[error("Invalid did.")]
    InvalidDid = 500,
    #[error("Invalid method.")]
    InvalidMethod = 501,
    #[error("Internal error.")]
    InternalError = 502,
    #[error("No Permission")]
    NoPermission = 504,
    #[error("Connect error, {0}")]
    ConnectError(rings_core::error::Error) = 600,
    #[error("Send message error: {0}")]
    SendMessage(rings_core::error::Error) = 601,
    #[error("vnode action error: {0}")]
    VNodeError(rings_core::error::Error) = 603,
    #[error("service register action error: {0}")]
    ServiceRegisterError(rings_core::error::Error) = 604,
    #[error("JsError: {0}")]
    JsError(String) = 700,
    #[error("Invalid message")]
    InvalidMessage = 800,
    #[error("Invalid http request: {0}")]
    HttpRequestError(String) = 801,
    #[error("Invalid data")]
    InvalidData = 802,
    #[error("Invalid service")]
    InvalidService = 803,
    #[error("Invalid address")]
    InvalidAddress = 804,
    #[error("Invalid auth data")]
    InvalidAuthData = 805,
    #[error("invalid headers")]
    InvalidHeaders = 806,
    #[error("Storage Error: {0}")]
    Storage(rings_core::error::Error) = 807,
    #[error("Swarm Error: {0}")]
    Swarm(rings_core::error::Error) = 808,
    #[error("Create File Error: {0}")]
    CreateFileError(String) = 900,
    #[error("Open File Error: {0}")]
    OpenFileError(String) = 901,
    #[error("acquire lock failed")]
    Lock = 902,
    #[error("serde json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error) = 1000,
    #[error("serde yaml error: {0}")]
    SerdeYamlError(#[from] serde_yaml::Error) = 1001,
    #[error("verify error: {0}")]
    VerifyError(String) = 1002,
    #[error("core error: {0}")]
    CoreError(#[from] rings_core::error::Error) = 1102,
    #[error("external singer error: {0}")]
    ExternalError(String) = 1202,
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

impl From<crate::prelude::rings_rpc::error::Error> for Error {
    fn from(e: crate::prelude::rings_rpc::error::Error) -> Self {
        match e {
            rings_rpc::error::Error::DecodeError => Error::DecodeError,
            rings_rpc::error::Error::EncodeError => Error::EncodeError,
            rings_rpc::error::Error::InvalidMethod => Error::InvalidMethod,
            rings_rpc::error::Error::RpcError(v) => Error::RemoteRpcError(v.to_string()),
            rings_rpc::error::Error::InvalidSignature => Error::InvalidData,
            rings_rpc::error::Error::InvalidHeaders => Error::InvalidHeaders,
            _ => Error::UnknownRpcError,
        }
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

#[cfg(feature = "browser")]
impl From<Error> for wasm_bindgen::JsValue {
    fn from(err: Error) -> Self {
        wasm_bindgen::JsValue::from_str(&err.to_string())
    }
}
