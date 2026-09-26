//! The swarm observer that records activity (feature `swarm-observer`).

use std::sync::Arc;

use rings_core::swarm::observer::LookupCorrelation;
use rings_core::swarm::observer::LookupKind;
use rings_core::swarm::observer::LookupOutcome;
use rings_core::swarm::observer::MessageObservation;
use rings_core::swarm::observer::SharedSwarmObserver;
use rings_core::swarm::observer::SwarmObserver;

use crate::activity::record_activity;

/// Observer that records activity on every observation: every message delivered, received or
/// stored, and every lookup started or finished.
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

/// An observer that records activity, to chain onto a test swarm or processor.
pub fn activity_observer() -> SharedSwarmObserver {
    Arc::new(ActivityObserver)
}
