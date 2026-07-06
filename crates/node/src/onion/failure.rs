//! Algebraic failure values carried by onion exit adapters.

use std::fmt;

use serde::Deserialize;
use serde::Serialize;

use crate::error::Error;

/// Recoverable failure reported by an onion exit to its client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum OnionExitFailure {
    /// The requested exit service is not enabled on the selected node.
    ExitUnavailable,
    /// The exit policy or local limiter denied the operation.
    PermissionDenied,
    /// The target name could not be resolved.
    ResolveTarget(String),
    /// The exit could not connect to the target.
    ConnectTarget(String),
    /// The exit failed while reading from the target.
    ReadTarget(String),
    /// The exit rejected a replayed payload.
    Replay,
    /// Any typed local error that has not yet been promoted to a dedicated wire variant.
    Protocol(String),
}

impl OnionExitFailure {
    /// Convert a local node error into a wire failure at the adapter boundary.
    pub fn from_error(error: &Error) -> Self {
        match error {
            Error::NoPermission => Self::PermissionDenied,
            Error::OnionRouteError(message) if message.contains("replayed") => Self::Replay,
            _ => Self::Protocol(error.to_string()),
        }
    }
}

impl fmt::Display for OnionExitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExitUnavailable => f.write_str("onion exit service is not enabled locally"),
            Self::PermissionDenied => Error::NoPermission.fmt(f),
            Self::ResolveTarget(message)
            | Self::ConnectTarget(message)
            | Self::ReadTarget(message)
            | Self::Protocol(message) => f.write_str(message),
            Self::Replay => f.write_str("replayed onion payload"),
        }
    }
}
