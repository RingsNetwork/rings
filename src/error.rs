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
    DecodedError,
    #[error("Encode error.")]
    EncodedError,
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
    #[error("Json serialize error.")]
    JsonSerializeError,
    #[error("Json Deserialize error.")]
    JsonDeserializeError,
    #[error("Invalid method.")]
    InvalidMethod,
    #[error("Internal error.")]
    InternalError,
    #[error("Connect with did error, {0}")]
    ConnectWithDidError(rings_core::err::Error),
    #[error("Connect error, {0}")]
    ConnectError(rings_core::err::Error),
    #[error("Send message error: {0}")]
    SendMessage(rings_core::err::Error),
    #[error("Build message body error: {0}")]
    MessagePayload(rings_core::err::Error),
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
    #[error("Not support message")]
    NotSupportMessage,
    #[error("Serialize error.")]
    SerializeError,
    #[error("Deserialize error.")]
    DeserializeError,
    #[error("invalid url.")]
    InvalidUrl,
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
}

impl Error {
    pub fn code(&self) -> i64 {
        let code = match self {
            Error::RemoteRpcError(_) => 0,
            Error::PendingTransport(_) => 1,
            Error::TransportNotFound => 2,
            Error::NewTransportError => 3,
            Error::CloseTransportError(_) => 4,
            Error::DecodedError => 5,
            Error::EncodedError => 6,
            Error::RegisterIceError(_) => 7,
            Error::CreateOffer(_) => 8,
            Error::CreateAnswer(_) => 9,
            Error::InvalidTransportId => 10,
            Error::InvalidDid => 11,
            Error::JsonSerializeError => 12,
            Error::JsonDeserializeError => 13,
            Error::InvalidMethod => 14,
            Error::InternalError => 15,
            Error::ConnectWithDidError(_) => 16,
            Error::ConnectError(_) => 17,
            Error::SendMessage(_) => 18,
            Error::MessagePayload(_) => 19,
            Error::NoPermission => 20,
            Error::VNodeError(_) => 21,
            Error::JsError(_) => 22,
            Error::HttpRequestError(_) => 23,
            Error::InvalidMessage => 24,
            Error::NotSupportMessage => 25,
            Error::SerializeError => 26,
            Error::DeserializeError => 27,
            Error::InvalidUrl => 28,
            Error::InvalidData => 29,
            Error::InvalidService => 30,
            Error::ServiceRegisterError(_) => 31,
            Error::InvalidAddress => 32,
            Error::InvalidAuthData => 33,
            Error::Storage(_) => 34,
            Error::Swarm(_) => 35,
            Error::CreateFileError(_) => 36,
            Error::OpenFileError(_) => 37,
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
