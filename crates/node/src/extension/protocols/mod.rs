#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in protocol implements [`Protocol`](crate::extension::ext::Protocol) and is
//! registered under its namespace, replacing the corresponding `BackendMessage`
//! variant.

pub mod echo;
#[cfg(feature = "browser")]
pub mod js;
pub mod relay;

use std::collections::HashMap;

use crate::error::Result;
use crate::extension::ext::Extensions;

/// Register the built-in protocol extensions into a registry.
///
/// The TCP and UDP relays start with empty service registries (safe: an `Open` to an
/// unknown service just closes); services are added via fixed config or runtime
/// registration. [`echo`] is a reference/test protocol and is intentionally **not**
/// registered by default (it replies to every message → would ping-pong between nodes).
pub fn register_builtins(extensions: &Extensions) -> Result<()> {
    // Native nodes back the relay with OS sockets; browsers with WebTransport. Same
    // namespaces and `Frame` vocabulary, platform-appropriate endpoint.
    #[cfg(not(feature = "browser"))]
    {
        extensions.register(relay::Relay::tcp(HashMap::new()))?;
        extensions.register(relay::Relay::udp(HashMap::new()))?;
    }
    #[cfg(feature = "browser")]
    {
        extensions.register(relay::WtRelay::tcp(HashMap::new()))?;
        extensions.register(relay::WtRelay::udp(HashMap::new()))?;
    }
    Ok(())
}
