/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors enum mapping global custom errors.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The incoming method name is not part of the Rings JSON-RPC surface.
    #[error("Invalid method.")]
    InvalidMethod,
    /// The JSON-RPC transport returned a method-level error.
    #[error("Rpc error: {0}")]
    RpcError(crate::jsonrpc::RpcError),
    /// The signed request metadata failed verification.
    #[error("Invalid signature.")]
    InvalidSignature,
    /// The request headers are missing required values or contain invalid values.
    #[error("Invalid headers.")]
    InvalidHeaders,
}
