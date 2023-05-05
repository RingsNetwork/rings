/// A wrap `Result` contains custom errors.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
/// Errors enum mapping global custom errors.
pub enum Error {
    #[error("Decode error.")]
    DecodeError,
    #[error("Encode error.")]
    EncodeError,
    #[error("Invalid method.")]
    InvalidMethod,
}
