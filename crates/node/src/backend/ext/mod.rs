#![warn(missing_docs)]
//! Unified, effect-separated protocol abstraction shared by `native` and `browser`.
//!
//! Design: *functional core, imperative shell*. A protocol author writes only a
//! **pure** state transition; all IO (sending, storage) is described as data
//! ([`Effect`]) and executed by the single impure boundary ([`Interpreter`]).
//!
//! Notation (used throughout the doc-comments here):
//!
//! ```text
//!   step : (Ctx S, Event) → Transition S        where   Transition S ≅ (S, [Effect])
//! ```
//!
//! Each event induces, via partial application, an endomorphism on the state paired
//! with an effect log. Such pairs form a monoid (state under endomorphism
//! composition, effects under list concatenation):
//!
//! ```text
//!   (f, a) ⊗ (g, b) = (g ∘ f,  a ⧺ b)            unit = (id, ε)
//! ```
//!
//! so a stream of events is reduced by `mconcat` / `fold` (the Writer-over-State
//! monad). Because `step` is pure it is total, testable and replayable; only
//! [`Interpreter::run`] touches the outside world.
//!
//! The abstraction and constraints are identical on both targets; the sole divergence
//! is the `Send` / `?Send` bound, isolated in [`MaybeSend`] and the usual `cfg_attr`
//! pair.
//!
//! ## Module layout
//!
//! - `envelope` — the wire [`Envelope`].
//! - `protocol` — the pure core: [`Event`], [`Effect`], [`Ctx`], [`Inbound`],
//!   [`Transition`], and the [`Protocol`] trait.
//! - `compute` — the effectful escape hatch ([`ComputeFn`] / [`ComputeServices`]).
//! - `interpreter` — the imperative shell ([`Interpreter`]), the only IO boundary.
//! - `registry` — type erasure ([`Handler`]) and the namespace router ([`Extensions`]).

mod compute;
mod envelope;
mod interpreter;
mod protocol;
mod registry;

pub use compute::ComputeFn;
pub use compute::ComputeServices;
pub use envelope::Envelope;
pub use interpreter::Interpreter;
pub use protocol::Ctx;
pub use protocol::Effect;
pub use protocol::Event;
pub use protocol::Inbound;
pub use protocol::Protocol;
pub use protocol::Transition;
pub use registry::DynHandler;
pub use registry::Extensions;
pub use registry::Handler;

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
