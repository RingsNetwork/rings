//! The first-frame deadline of a detached send: how long a payload queued apart from its sender
//! (a forward, a relay, a probe) waits for its first frame's admission, credit wait included.
//! A payload the next peer's backpressure holds longer is dropped as local backpressure.
//!
//! Test builds can hold the deadline on the current thread, so a witness of the inbound-locality
//! law observes a send stuck behind withheld credit for as long as the witness runs, instead of
//! racing the short dummy deadline.

use std::time::Duration;

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::deadline_hold;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::deadline_hold::DeadlineHold;
use crate::swarm::transport::TRANSPORT_TIMEOUT_PROFILE;

/// The deadline, unless a test holds it.
pub(super) fn first_frame_deadline() -> Duration {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if let Some(held) = deadline_hold::held_deadline(&HELD) {
        return held;
    }
    TRANSPORT_TIMEOUT_PROFILE.first_frame_admission
}

#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
thread_local! {
    /// Whether this thread's detached sends wait out a held first-frame deadline.
    static HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test builds: hold the first-frame deadline of every detached send this thread starts, until
/// the returned guard is dropped.
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) fn hold_first_frame_deadlines_for_test() -> DeadlineHold {
    deadline_hold::hold(&HELD)
}
