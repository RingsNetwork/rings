#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in protocol implements [`Protocol`](crate::backend::ext::Protocol) and is
//! registered under its namespace, replacing the corresponding `BackendMessage`
//! variant.

pub mod echo;
#[cfg(feature = "browser")]
pub mod js;

use crate::backend::ext::Extensions;
use crate::error::Result;

/// Register the built-in protocol extensions into a registry.
///
/// Built-ins are added here as they are ported (TCP / HTTP / SNARK). [`echo`] is a
/// reference/test protocol and is intentionally **not** registered by default: it
/// replies to every message, which would ping-pong endlessly between nodes.
pub fn register_builtins(_extensions: &Extensions) -> Result<()> {
    Ok(())
}
