//! A bunch of wrap errors.
use crate::prelude::rings_core;

/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
/// Errors enum mapping global custom errors.
pub enum Error {
    #[error("Connect remote rpc server failed: {0}.")]
    RemoteRpcError(String),
    #[error("Pending Transport error: {0}.")]
    PendingTransport(rings_core::err::Error),
    #[error("Transport not found.")]
    TransportNotFound,
    #[error("Create Transport error.")]
    NewTransportError,
    #[error("Close Transport error: {0}.")]
    CloseTransportError(rings_core::err::Error),
    #[error("Decode error.")]
    DecodeError,
    #[error("Encode error.")]
    EncodeError,
    #[error("Register ICE error: {0}.")]
    RegisterIceError(rings_core::err::Error),
    #[error("Create offer info failed: {0}.")]
    CreateOffer(rings_core::err::Error),
    #[error("Create answer info failed: {0}.")]
    CreateAnswer(rings_core::err::Error),
    #[error("Invalid transport id.")]
    InvalidTransportId,
    #[error("Invalid did.")]
    InvalidDid,
    #[error("Invalid method.")]
    InvalidMethod,
    #[error("Internal error.")]
    InternalError,
    #[error("Connect error, {0}")]
    ConnectError(rings_core::err::Error),
    #[error("Send message error: {0}")]
    SendMessage(rings_core::err::Error),
    #[error("No Permission")]
    NoPermission,
    #[error("vnode action error: {0}")]
    VNodeError(rings_core::err::Error),
    #[error("service register action error: {0}")]
    ServiceRegisterError(rings_core::err::Error),
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
    Storage(rings_core::err::Error),
    #[error("Swarm Error: {0}")]
    Swarm(rings_core::err::Error),
    #[error("Create File Error: {0}")]
    CreateFileError(String),
    #[error("Open File Error: {0}")]
    OpenFileError(String),
    #[error("acquire lock failed")]
    Lock,
}

impl Error {
    pub fn code(&self) -> i64 {
        let code = match self {
            Error::RemoteRpcError(_) => 1,
            Error::ConnectError(_) => 1,
            Error::HttpRequestError(_) => 1,
            Error::PendingTransport(_) => 2,
            Error::TransportNotFound => 3,
            Error::NewTransportError => 4,
            Error::CloseTransportError(_) => 5,
            Error::EncodeError => 6,
            Error::DecodeError => 7,
            Error::RegisterIceError(_) => 8,
            Error::CreateOffer(_) => 9,
            Error::CreateAnswer(_) => 10,
            Error::InvalidTransportId => 11,
            Error::InvalidDid => 12,
            Error::InvalidMethod => 13,
            Error::SendMessage(_) => 14,
            Error::NoPermission => 15,
            Error::VNodeError(_) => 16,
            Error::ServiceRegisterError(_) => 17,
            Error::InvalidData => 18,
            Error::InvalidMessage => 19,
            Error::InvalidService => 20,
            Error::InvalidAddress => 21,
            Error::InvalidAuthData => 22,
            Error::InternalError => 0,
            Error::CreateFileError(_) => 0,
            Error::OpenFileError(_) => 0,
            Error::JsError(_) => 0,
            Error::Swarm(_) => 0,
            Error::Storage(_) => 0,
            Error::Lock => 0,
        };
        -32000 - code
    }
}

impl From<Error> for jsonrpc_core::Error {
    fn from(e: Error) -> Self {
        Self {
            code: jsonrpc_core::ErrorCode::ServerError(e.code()),
            message: e.to_string(),
            data: None,
        }
    }
}
