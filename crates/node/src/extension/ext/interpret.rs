//! The imperative shell of an extension: [`Interpret`] runs a protocol's **own** effects.
//!
//! Each extension registers a `(Protocol, Interpret)` pair. The interpreter is the only
//! place IO happens for that extension; it is handed a **namespace-scoped** capability
//! ([`EffectScope`](super::EffectScope)) — overlay `send` and `did`, confined to the
//! interpreter's own namespace. An extension that owns OS resources (e.g. the relay's sockets)
//! receives a lifecycle scope explicitly for its long-lived tasks, so the core never depends on
//! transport internals.

use bytes::Bytes;

use super::EffectScope;
use crate::error::Result;

/// Runs the effects produced by a protocol's pure `step`. `run` returns the payloads to
/// re-inject into the router; each is re-delivered to **this** protocol's own namespace with
/// `from = this node` — the router sets the provenance, so a shell can forge neither a
/// namespace nor a remote `from`. A hard failure is an `Err`. Defined per-effect outcomes (e.g.
/// "addressed no live session") are the extension's own concern and surfaced however it likes
/// (its own return shapes / tests), not a core enum.
#[cfg_attr(all(feature = "browser", target_family = "wasm"), async_trait::async_trait(?Send))]
#[cfg_attr(
    not(all(feature = "browser", target_family = "wasm")),
    async_trait::async_trait
)]
pub trait Interpret {
    /// The effect algebra this shell interprets — the same as its protocol's `Effect`.
    type Effect;

    /// Run one effect against the scoped capability, returning self-injected payloads (each
    /// re-decoded by this same protocol).
    ///
    /// The state transition is committed before this method starts. Effects are attempted in
    /// their transition order; an `Err` stops the current transition's remaining effects and
    /// does not roll state back. A protocol that needs retry or compensation therefore models it
    /// as a later typed event/transition, rather than relying on an interpreter retry.
    async fn run(&self, scope: &EffectScope, effect: Self::Effect) -> Result<Vec<Bytes>>;
}
