//! Activity sources of node's test processors, native and browser.
//!
//! The activity cell and the activity-woken probe live in [`rings_test_support::activity`];
//! this module wires node's test processors to it. A processor built with
//! [`ProcessorBuilder::observer`](crate::processor::ProcessorBuilder::observer) and
//! [`activity_observer`] records activity on every message delivered, received or stored and on
//! every lookup event, so helpers probe the state they need on activity instead of on a timer.

#[cfg(feature = "node")]
use std::future::Future;
use std::sync::Arc;
#[cfg(feature = "node")]
use std::time::Duration;

use rings_core::swarm::observer::LookupCorrelation;
use rings_core::swarm::observer::LookupKind;
use rings_core::swarm::observer::LookupOutcome;
use rings_core::swarm::observer::MessageObservation;
use rings_core::swarm::observer::SharedSwarmObserver;
use rings_core::swarm::observer::SwarmObserver;
pub(crate) use rings_test_support::activity::record_activity;

/// Observer that records activity on every observation.
struct ActivityObserver;

impl SwarmObserver for ActivityObserver {
    fn observe_message(&self, _observation: MessageObservation) {
        record_activity();
    }

    fn lookup_started(&self, _kind: LookupKind, _correlation: LookupCorrelation) {
        record_activity();
    }

    fn lookup_finished(
        &self,
        _kind: LookupKind,
        _correlation: LookupCorrelation,
        _outcome: LookupOutcome,
    ) {
        record_activity();
    }
}

/// The observer every test processor chains after its own `Observability`.
pub(crate) fn activity_observer() -> SharedSwarmObserver {
    Arc::new(ActivityObserver)
}

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
