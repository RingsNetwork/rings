#![warn(missing_docs)]
//! Built-in protocol extensions.
//!
//! Each built-in is a `(Protocol, Interpret)` pair registered under its namespace. The
//! relay's pure model is one generic [`relay::Relay`]; only its interpreter differs by
//! platform (`NativeRelay` / `WtRelay`).

pub mod echo;
#[cfg(feature = "browser")]
pub mod js;
pub mod relay;

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Result;
use crate::extension::ext::Extensions;

/// Register the built-in protocol extensions into a registry.
///
/// The TCP and UDP relays start with empty service registries (an `Open` to an unknown
/// service just replies close); services are added via fixed config or runtime
/// registration. [`echo`] is a reference/test protocol and is intentionally **not**
/// registered by default (it replies to every message → would ping-pong between nodes).
///
/// Native nodes back the relay with OS sockets, browsers with WebTransport — same
/// namespaces and [`Frame`](crate::extension::transport::Frame) vocabulary, platform
/// interpreter. The relay's pure protocol and engine table both live inside this one
/// extension, so the core never depends on transport internals. The engine is created and
/// owned here (shared into the interpreters); a client-side [`relay::RelayHandle`] over the
/// same engine is returned for opening tunnels / registering services.
#[cfg(not(feature = "browser"))]
pub fn register_builtins(extensions: &Extensions) -> Result<relay::RelayHandle> {
    let engine = Arc::new(crate::extension::transport::engine::TransportSessions::new());
    extensions.register(
        relay::Relay::tcp(HashMap::new()),
        relay::NativeRelay::new(engine.clone()),
    )?;
    extensions.register(
        relay::Relay::udp(HashMap::new()),
        relay::NativeRelay::new(engine.clone()),
    )?;
    Ok(relay::RelayHandle::new(engine, extensions.core()))
}

/// Register the built-in protocol extensions into a registry (browser / WebTransport).
#[cfg(feature = "browser")]
pub fn register_builtins(extensions: &Extensions) -> Result<relay::RelayHandle> {
    let engine = Arc::new(crate::extension::transport::wt::WtSessions::new());
    extensions.register(
        relay::Relay::tcp(HashMap::new()),
        relay::WtRelay::new(engine.clone()),
    )?;
    extensions.register(
        relay::Relay::udp(HashMap::new()),
        relay::WtRelay::new(engine),
    )?;
    Ok(relay::RelayHandle::new(extensions.core()))
}
