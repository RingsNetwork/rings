#![warn(missing_docs)]
//! The imperative shell of an extension: [`Interpret`] runs a protocol's **own** effects.
//!
//! Each extension registers a `(Protocol, Interpret)` pair. The interpreter is the only
//! place IO happens for that extension; it is handed a **namespace-scoped** capability
//! ([`Scope`](super::Scope)) — overlay `send`, `did`, and self-`inject`, all confined to the
//! interpreter's own namespace. An extension that owns OS resources (e.g. the relay's sockets)
//! keeps them inside its interpreter, so the core never depends on transport internals.

use bytes::Bytes;

use super::Scope;
use crate::error::Result;

/// Runs the effects produced by a protocol's pure `step`. `run` returns the payloads to
/// re-inject into the router; each is re-delivered to **this** protocol's own namespace with
/// `from = this node` — the router sets the provenance, so a shell can forge neither a
/// namespace nor a remote `from`. A hard failure is an `Err`. Defined per-effect outcomes (e.g.
/// "addressed no live session") are the extension's own concern and surfaced however it likes
/// (its own return shapes / tests), not a core enum.
#[cfg_attr(feature = "browser", async_trait::async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait::async_trait)]
pub trait Interpret {
    /// The effect algebra this shell interprets — the same as its protocol's `Effect`.
    type Effect;

    /// Run one effect against the scoped capability, returning self-injected payloads (each
    /// re-decoded by this same protocol).
    async fn run(&self, scope: &Scope, effect: Self::Effect) -> Result<Vec<Bytes>>;
}
