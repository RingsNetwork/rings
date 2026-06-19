#![warn(missing_docs)]
//! Unified, effect-separated protocol abstraction shared by `native` and `browser`.
//!
//! Design: *functional core, imperative shell*. A protocol author writes only a **pure**
//! state transition over its **own** typed events and effects; all IO is performed by the
//! extension's own [`Interpret`] shell, which is handed a namespace-scoped capability
//! ([`Scope`]) — `send`/`inject` confined to the interpreter's own namespace.
//!
//! Notation (used throughout the doc-comments here):
//!
//! ```text
//!   decode : Wire ⇀ Event
//!   step   : (Ctx S, Event) → Transition (S, Effect)     where Transition (S,E) ≅ (S, [E])
//! ```
//!
//! The effect algebra is **not** global: each extension defines `Protocol::Effect` and the
//! interpreter that runs it. The core owns no `Effect` enum — adding an extension never
//! touches the core, and a protocol can only emit its own effects (no global command bus).
//!
//! `step` is pure (no IO, clocks, globals) and total over well-typed events; the decode
//! boundary makes "undecodable/foreign input" an explicit [`Reject`] instead of a silent
//! no-op. The abstraction is identical on both targets; the sole divergence is the `Send` /
//! `?Send` bound, isolated in [`MaybeSend`].
//!
//! ## Module layout
//!
//! - `envelope` — the wire [`Envelope`].
//! - `protocol` — the pure core: [`Wire`]/[`Reject`] (decode boundary), [`Ctx`],
//!   `Inbound` (router-internal), [`Transition`], and the [`Protocol`] trait.
//! - `interpret` — the per-extension imperative shell ([`Interpret`]).
//! - `registry` — the scoped capability [`Scope`] handed to shells, plus the router-internal
//!   `Core` / `Handler` and the namespace registry ([`Extensions`]).

mod envelope;
mod interpret;
mod protocol;
mod registry;

pub use envelope::Envelope;
pub use interpret::Interpret;
pub use protocol::Ctx;
// Router internals — not part of the extension-author API (which is `Protocol` / `Interpret` /
// `Scope` / `Transition` / …). Crate-visible only, so the old ambient `Core`/`Inbound` surface
// cannot be used to bypass the scoped-capability boundary. `Handler`/`DynHandler` stay private
// to `registry` (the erased router ABI; protocol authors never name them).
pub(crate) use protocol::Inbound;
pub use protocol::Protocol;
pub use protocol::Reject;
pub use protocol::Transition;
pub use protocol::Wire;
pub(crate) use registry::Core;
pub use registry::Extensions;
pub use registry::Scope;

/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
///
/// Lets the pure-core types be written once; the `Send`-ness divergence (browser futures
/// are not `Send`) is confined here. `∀ T` on browser; `Send + Sync` elsewhere.
#[cfg(not(feature = "browser"))]
pub trait MaybeSend: Send + Sync {}
#[cfg(not(feature = "browser"))]
impl<T: Send + Sync> MaybeSend for T {}
/// Auto-trait bound that is `Send + Sync` on native and empty on browser.
#[cfg(feature = "browser")]
pub trait MaybeSend {}
#[cfg(feature = "browser")]
impl<T> MaybeSend for T {}
