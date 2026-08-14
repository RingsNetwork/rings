//! Error facts owned by the SNARK extension.

use crate::error::Error;

/// SNARK extension failures before they cross the generic extension boundary.
#[derive(Debug, thiserror::Error)]
pub enum SnarkError {
    /// The proving backend returned an error.
    #[error("Snark error: {0}")]
    Backend(#[from] rings_snark::error::Error),
    /// The requested curve does not match the task.
    #[error("Snark curve not match")]
    CurveNotMatch,
    /// Handling a protocol message failed.
    #[error("Snark handle message error: {0}")]
    HandleMessage(String),
    /// Converting a JavaScript bigint into a prime-field element was out of range.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    #[error("range error when converting js_sys::BigInt to PrimeField: {0}")]
    FieldRange(String),
    /// Converting a JavaScript bigint produced an empty representation.
    #[cfg(all(feature = "browser", target_family = "wasm"))]
    #[error("Failed to load bigint to repr string, it's empty")]
    BigIntValueEmpty,
    /// Loading a string as a prime-field element failed.
    #[error("Failed to load string to PrimeField")]
    FailedToLoadField,
}

impl From<SnarkError> for Error {
    fn from(error: SnarkError) -> Self {
        Error::ExtensionError(error.to_string())
    }
}

impl From<rings_snark::error::Error> for Error {
    fn from(error: rings_snark::error::Error) -> Self {
        SnarkError::Backend(error).into()
    }
}
