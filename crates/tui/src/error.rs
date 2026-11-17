//! Error types for `rings-tui`.

use std::path::PathBuf;

/// Errors returned by the Rings TUI library and binary.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    /// Terminal or file I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON-RPC client error.
    #[error("RPC error: {0}")]
    Rpc(String),

    /// HTTP status endpoint request error.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// Configuration file could not be read or parsed.
    #[error("failed to load config from {path}: {reason}")]
    Config {
        /// Path of the configuration file.
        path: PathBuf,
        /// Explanation of the failure.
        reason: String,
    },

    /// API token file could not be read or validated.
    #[error("failed to load API token from {path}: {reason}")]
    ApiToken {
        /// Path of the token file.
        path: PathBuf,
        /// Explanation of the failure.
        reason: String,
    },

    /// Invalid input supplied to a command or form.
    #[error("invalid command input: {0}")]
    InvalidInput(String),
}

/// Result alias for `rings-tui` operations.
pub type Result<T> = std::result::Result<T, TuiError>;

impl From<rings_rpc::jsonrpc::RpcError> for TuiError {
    fn from(error: rings_rpc::jsonrpc::RpcError) -> Self {
        Self::Rpc(error.to_string())
    }
}

impl From<std::convert::Infallible> for TuiError {
    fn from(never: std::convert::Infallible) -> Self {
        match never {}
    }
}
