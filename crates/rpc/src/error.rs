/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Invalid method.")]
    InvalidMethod,
    #[error("Rpc error: {0}")]
    RpcError(crate::jsonrpc::RpcError),
    #[error("Invalid signature.")]
    InvalidSignature,
    #[error("Invalid headers.")]
    InvalidHeaders,
}
