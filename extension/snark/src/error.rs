//! Error facts owned by the SNARK extension.

use rings_node::error::Error;

/// Result type owned by the SNARK extension.
pub type Result<T> = std::result::Result<T, SnarkError>;

/// SNARK extension failures before they cross the generic extension boundary.
#[derive(Debug, thiserror::Error)]
pub enum SnarkError {
    /// The proving backend returned an error.
    #[error("Snark error: {0}")]
    Backend(#[from] rings_snark::error::Error),
    /// The Rings node extension boundary returned an error.
    #[error("Node extension error: {0}")]
    Node(#[from] Error),
    /// A task id could not be parsed.
    #[error("Invalid task id: {0}")]
    TaskId(#[from] uuid::Error),
    /// JSON serialization failed.
    #[error("Snark json error: {0}")]
    Json(#[from] serde_json::Error),
    /// Binary serialization failed.
    #[error("Snark encode error")]
    Encode,
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

#[cfg(all(feature = "browser", target_family = "wasm"))]
impl From<SnarkError> for wasm_bindgen::JsValue {
    fn from(error: SnarkError) -> Self {
        wasm_bindgen::JsValue::from_str(&error.to_string())
    }
}
