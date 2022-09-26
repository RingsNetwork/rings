use crate::prelude::rings_core;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
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
    #[error("Send mesage error: {0}")]
    SendMessage(rings_core::err::Error),
    #[error("Build message body error: {0}")]
    MessagePayload(rings_core::err::Error),
    #[error("No Permission")]
    NoPermission,
    #[error("vnode action error: {0}")]
    VNodeError(rings_core::err::Error),
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
