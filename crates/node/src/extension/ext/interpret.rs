#![warn(missing_docs)]
//! The imperative shell of an extension: [`Interpret`] runs a protocol's **own** effects.
//!
//! Each extension registers a `(Protocol, Interpret)` pair. The interpreter is the only
//! place IO happens for that extension; it is handed the small core capability surface
//! ([`Core`](super::Core)) — overlay `send`, `did`, and `inject` (feed an event back into
//! the router). An extension that owns OS resources (e.g. the relay's sockets) keeps them
//! inside its interpreter, so the core never depends on transport internals.

use super::Core;
use super::Inbound;
use crate::error::Result;

/// Runs the effects produced by a protocol's pure `step`. `run` returns the events to
/// re-inject into the router (the effect's event trace); a hard failure is an `Err`.
/// Defined per-effect outcomes (e.g. "addressed no live session") are the extension's own
/// concern and surfaced however it likes (its own return shapes / tests), not a core enum.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait Interpret {
    /// The effect algebra this shell interprets — the same as its protocol's `Effect`.
    type Effect;

    /// Run one effect against the core capabilities, returning re-injected inbounds.
    async fn run(&self, core: &Core, effect: Self::Effect) -> Result<Vec<Inbound>>;
}
