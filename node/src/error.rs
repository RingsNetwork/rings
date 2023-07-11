//! A bunch of wrap errors.
use crate::prelude::jsonrpc_core;
use crate::prelude::rings_core;

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
#[repr(u16)]
pub enum Error {
    #[error("Connect remote rpc server failed: {0}.")]
    RemoteRpcError(String) = 0,
    #[error("Unknown rpc error.")]
    UnknownRpcError,
    #[error("Pending Transport error: {0}.")]
    PendingTransport(rings_core::error::Error),
    #[error("Transport not found.")]
    TransportNotFound,
    #[error("Create Transport error: {0}.")]
    NewTransportError(rings_core::error::Error),
    #[error("Close Transport error: {0}.")]
    CloseTransportError(rings_core::error::Error),
    #[error("Decode error.")]
    DecodeError,
    #[error("Encode error.")]
    EncodeError,
    #[error("WASM compile error: {0}")]
    WasmCompileError(String),
    #[error("BackendMessage RwLock Error")]
    WasmBackendMessageRwLockError,
    #[error("WASM instantiation error.")]
    WasmInstantiationError,
    #[error("WASM export error.")]
    WasmExportError,
    #[error("WASM runtime error: {0}")]
    WasmRuntimeError(String),
    #[error("WASM global memory mutex error.")]
    WasmGlobalMemoryLockError,
    #[error("WASM failed to load file.")]
    WasmFailedToLoadFile,
    #[error("Create offer info failed: {0}.")]
    CreateOffer(rings_core::error::Error),
    #[error("Answer offer info failed: {0}.")]
    AnswerOffer(rings_core::error::Error),
    #[error("Accept answer info failed: {0}.")]
    AcceptAnswer(rings_core::error::Error),
    #[error("Invalid transport id.")]
    InvalidTransportId,
    #[error("Invalid did.")]
    InvalidDid,
    #[error("Invalid method.")]
    InvalidMethod,
    #[error("Internal error.")]
    InternalError,
    #[error("Connect error, {0}")]
    ConnectError(rings_core::error::Error),
    #[error("Send message error: {0}")]
    SendMessage(rings_core::error::Error),
    #[error("No Permission")]
    NoPermission,
    #[error("vnode action error: {0}")]
    VNodeError(rings_core::error::Error),
    #[error("service register action error: {0}")]
    ServiceRegisterError(rings_core::error::Error),
    #[error("JsError: {0}")]
    JsError(String),
    #[error("Invalid http request: {0}")]
    HttpRequestError(String),
    #[error("Invalid message")]
    InvalidMessage,
    #[error("Invalid data")]
    InvalidData,
    #[error("Invalid service")]
    InvalidService,
    #[error("Invalid address")]
    InvalidAddress,
    #[error("Invalid auth data")]
    InvalidAuthData,
    #[error("Storage Error: {0}")]
    Storage(rings_core::error::Error),
    #[error("Swarm Error: {0}")]
    Swarm(rings_core::error::Error),
    #[error("Create File Error: {0}")]
    CreateFileError(String),
    #[error("Open File Error: {0}")]
    OpenFileError(String),
    #[error("acquire lock failed")]
    Lock,
    #[error("invalid headers")]
    InvalidHeaders,
    #[error("serde json error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
    #[error("verify error: {0}")]
    VerifyError(String),
}

impl Error {
    fn discriminant(&self) -> u16 {
        // SAFETY: Because `Self` is marked `repr(u16)`, its layout is a `repr(C)` `union`
        // between `repr(C)` structs, each of which has the `u16` discriminant as its first
        // field, so we can read the discriminant without offsetting the pointer.
        // This code is copy from
        // ref: https://doc.rust-lang.org/std/mem/fn.discriminant.html
        // And we modify it from [u8] to [u16], this is work because
        // repr(C) is equivalent to one of repr(u*) (see the next section) for
        // fieldless enums.
        // ref: https://doc.rust-lang.org/nomicon/other-reprs.html
        unsafe { *<*const _>::from(self).cast::<u16>() }
    }

    pub fn code(&self) -> u16 {
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
        assert_eq!(err.code(), 0);
    }
}
