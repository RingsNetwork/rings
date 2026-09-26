//! Activity sources of node's test processors, native and browser.
//!
//! The activity cell and the activity-woken probe live in [`rings_test_support::activity`];
//! this module wires node's test processors to it. A processor built with
//! [`ProcessorBuilder::observer`](crate::processor::ProcessorBuilder::observer) and
//! [`activity_observer`] records activity on every message delivered, received or stored and on
//! every lookup event, so helpers probe the state they need on activity instead of on a timer.

#[cfg(feature = "node")]
use std::future::Future;
#[cfg(feature = "node")]
use std::time::Duration;

#[cfg(feature = "node")]
pub(crate) use rings_test_support::activity::record_activity;
/// The observer every test processor chains after its own `Observability`.
pub(crate) use rings_test_support::observer::activity_observer;

/// [`rings_test_support::activity::probe_on_activity`] with node's error type (native
/// processor tests; the browser tests await their own event logs).
#[cfg(feature = "node")]
pub(crate) async fn probe_on_activity<T, F>(
    label: &str,
    hang_guard: Duration,
    probe: impl FnMut() -> F,
) -> crate::error::Result<T>
where
    F: Future<Output = crate::error::Result<Option<T>>>,
{
    rings_test_support::activity::probe_on_activity(label, hang_guard, probe).await
}
