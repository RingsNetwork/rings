//! Utils for ring-core.

mod id;
mod slow_lock;
mod time;

pub(crate) use id::new_uuid;
pub use slow_lock::slow_lock_hold_count;
pub use slow_lock::slow_lock_wait_count;
pub use slow_lock::SlowLockGuard;
pub use slow_lock::SLOW_ASYNC_LOCK_THRESHOLD;
pub use time::get_epoch_ms;
pub(crate) use time::get_epoch_ms_i64;
pub(crate) use time::sleep;
pub(crate) use time::try_sleep;
pub(crate) use time::Instant;

#[cfg(all(feature = "wasm", target_family = "wasm"))]
/// JavaScript value conversion helpers used by the WASM bindings.
pub mod js_value;

mod witness;
pub(crate) use witness::GenerationWitness;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use witness::Witness;
