/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Decode error.")]
    DecodeError,
    #[error("Encode error.")]
    EncodeError,
    #[error("Invalid method.")]
    InvalidMethod,
    #[error("Rpc error: {0}")]
    RpcError(crate::jsonrpc_client::client::RpcError),
    #[error("Invalid signature.")]
    InvalidSignature,
    #[error("Invalid headers.")]
    InvalidHeaders,
}
