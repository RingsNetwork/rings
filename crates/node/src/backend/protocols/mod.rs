#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in protocol implements [`Protocol`](crate::backend::ext::Protocol) and is
//! registered under its namespace, replacing the corresponding `BackendMessage`
//! variant.

pub mod echo;
#[cfg(feature = "browser")]
pub mod js;
pub mod tcp;

use crate::backend::ext::Extensions;
use crate::error::Result;

/// Register the built-in protocol extensions into a registry.
///
/// More built-ins are added here as they are ported (HTTP / UDP / SNARK). [`echo`] is
/// a reference/test protocol and is intentionally **not** registered by default: it
/// replies to every message, which would ping-pong endlessly between nodes. The TCP
/// relay starts with an empty service registry (safe: an `Open` to an unknown service
/// just closes); services are added via fixed config or runtime registration.
pub fn register_builtins(extensions: &Extensions) -> Result<()> {
    extensions.register(tcp::Tcp::default())?;
    Ok(())
}
