//! Test holds of the transport's real-time deadlines.
//!
//! The dummy profile's deadlines are tens to hundreds of milliseconds of real time. A test that
//! witnesses a state one of them bounds (a frame parked at a gate, a delivery held open) holds
//! that family of deadlines on its thread for as long as the witness runs, so the witness is
//! decided by the events it waits for, never by how fast the host runs:
//!
//! ```text
//!   held_deadline(s) = Some(HELD_DEADLINE)   while a hold of s lives on this thread
//!                    = None                  otherwise
//! ```
//!
//! A switch is thread-local. Native dummy tests run on Tokio's current-thread runtime, so the
//! test's thread is the thread of every transfer it starts.

use std::cell::Cell;
use std::thread::LocalKey;
use std::time::Duration;

/// A held deadline: longer than any test runs.
const HELD_DEADLINE: Duration = Duration::from_secs(3_600);

/// The per-thread switch of one family of deadlines.
pub(super) type DeadlineSwitch = LocalKey<Cell<bool>>;

/// [`HELD_DEADLINE`] while `switch` is held on this thread; `None` leaves the deadline as is.
pub(super) fn held_deadline(switch: &'static DeadlineSwitch) -> Option<Duration> {
    switch.with(Cell::get).then_some(HELD_DEADLINE)
}

/// Hold `switch` on this thread until the returned guard is dropped.
pub(super) fn hold(switch: &'static DeadlineSwitch) -> DeadlineHold {
    switch.with(|held| held.set(true));
    DeadlineHold(switch)
}

/// Releases the hold of [`hold`] when dropped.
pub(crate) struct DeadlineHold(&'static DeadlineSwitch);

impl Drop for DeadlineHold {
    fn drop(&mut self) {
        self.0.with(|held| held.set(false));
    }
}
