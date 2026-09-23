//! Thread-safety bounds that differ between the two runtimes.
//!
//! Native code runs on a multi-threaded Tokio executor, so anything it schedules or shares
//! must cross threads; the browser executor is the single JavaScript thread, whose futures
//! and handles (`Rc`, `JsValue`) are never `Send`. Each bound below is written once by its
//! consumers and resolves per target:
//!
//! ```text
//!   MaybeSend      ≅  Send          (native)  |  ⊤ (browser)
//!   MaybeSendSync  ≅  Send ∧ Sync   (native)  |  ⊤ (browser)
//! ```
//!
//! `MaybeSend` bounds what is *moved* onto the executor (a spawned future and its output);
//! `MaybeSendSync` bounds what is *shared* between tasks (protocol state, connections).

/// Bound on values moved onto the executor: `Send` on native, unconstrained in the browser.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub trait MaybeSend: Send {}
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
impl<T: Send + ?Sized> MaybeSend for T {}

/// Bound on values moved onto the executor: `Send` on native, unconstrained in the browser.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub trait MaybeSend {}
#[cfg(all(feature = "browser", target_family = "wasm"))]
impl<T: ?Sized> MaybeSend for T {}

/// Bound on values shared between tasks: `Send + Sync` on native, unconstrained in the browser.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
pub trait MaybeSendSync: Send + Sync {}
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
impl<T: Send + Sync + ?Sized> MaybeSendSync for T {}

/// Bound on values shared between tasks: `Send + Sync` on native, unconstrained in the browser.
#[cfg(all(feature = "browser", target_family = "wasm"))]
pub trait MaybeSendSync {}
#[cfg(all(feature = "browser", target_family = "wasm"))]
impl<T: ?Sized> MaybeSendSync for T {}
