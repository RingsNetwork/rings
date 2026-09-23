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
//!
//! A trait object cannot name either trait (only auto traits may follow the principal trait
//! of a `dyn`), so the same resolution is offered in type position by two macros:
//!
//! ```text
//!   maybe_send!(dyn T)       ≅  dyn T + Send          (native)  |  dyn T (browser)
//!   maybe_send_sync!(dyn T)  ≅  dyn T + Send + Sync   (native)  |  dyn T (browser)
//! ```
//!
//! Each macro is selected by *this* crate's cfg when `rings-runtime` is compiled, so every
//! consumer's trait objects agree with its trait bounds instead of re-deriving the target
//! split under a predicate of its own.

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

/// A trait object that is `Send` on native and unconstrained in the browser.
///
/// `maybe_send!(dyn Future<Output = ()> + 'static)` names the object type a native executor
/// may move between threads; see the module documentation.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
#[macro_export]
macro_rules! maybe_send {
    (dyn $($object:tt)+) => { dyn $($object)+ + ::core::marker::Send };
}

/// A trait object that is `Send` on native and unconstrained in the browser.
#[cfg(all(feature = "browser", target_family = "wasm"))]
#[macro_export]
macro_rules! maybe_send {
    (dyn $($object:tt)+) => { dyn $($object)+ };
}

/// A trait object that is `Send + Sync` on native and unconstrained in the browser.
///
/// `Arc<maybe_send_sync!(dyn Handler)>` names a handler shared between native tasks; see the
/// module documentation.
#[cfg(not(all(feature = "browser", target_family = "wasm")))]
#[macro_export]
macro_rules! maybe_send_sync {
    (dyn $($object:tt)+) => { dyn $($object)+ + ::core::marker::Send + ::core::marker::Sync };
}

/// A trait object that is `Send + Sync` on native and unconstrained in the browser.
#[cfg(all(feature = "browser", target_family = "wasm"))]
#[macro_export]
macro_rules! maybe_send_sync {
    (dyn $($object:tt)+) => { dyn $($object)+ };
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    fn is_send<T: Send + ?Sized>() {}
    fn is_send_sync<T: Send + Sync + ?Sized>() {}

    /// Law: on native the macros resolve to the same bounds as the traits they mirror.
    #[test]
    fn test_native_trait_objects_carry_the_native_bounds() {
        is_send::<maybe_send!(dyn Future<Output = ()> + 'static)>();
        is_send_sync::<maybe_send_sync!(dyn std::error::Error)>();
        is_send::<Pin<Box<maybe_send!(dyn Future<Output = u8>)>>>();
    }
}
