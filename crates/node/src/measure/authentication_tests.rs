use std::sync::Arc;

use rings_core::dht::Did;
use rings_core::measure::Measure;
use rings_core::storage::MemStorage;
use rings_measure::ApplyOutcome;
use rings_measure::Authentication;
use rings_measure::MeasurementEvent;
use rings_measure::UnixTime;
use rings_measure::DEFAULT_MAX_RETAINED_PEERS;

use super::lock_or_recover;
use super::MeasureClock;
use super::PeriodicMeasure;

struct FixedClock;

impl MeasureClock for FixedClock {
    fn now(&self) -> UnixTime {
        UnixTime::from_secs(10)
    }
}

#[tokio::test]
async fn unauthenticated_identity_flood_does_not_consume_retained_peer_capacity() {
    let measure =
        PeriodicMeasure::new_with_clock(Box::new(MemStorage::new()), Arc::new(FixedClock))
            .await
            .unwrap_or_else(|error| panic!("measurement must initialize: {error}"));

    for raw_peer in 0..=DEFAULT_MAX_RETAINED_PEERS.get() {
        let raw_peer = u32::try_from(raw_peer)
            .unwrap_or_else(|error| panic!("fixture peer id must fit u32: {error}"));
        assert_eq!(
            measure
                .record(
                    Did::from(raw_peer),
                    Authentication::Unauthenticated,
                    MeasurementEvent::FailedToSend,
                )
                .await,
            Ok(ApplyOutcome::IgnoredUnattributable)
        );
    }

    {
        let runtime = lock_or_recover(&measure.state.runtime);
        assert!(runtime.ledger.is_empty());
        assert!(!runtime.dirty);
    }

    let authenticated_peer = Did::from(u32::MAX);
    assert_eq!(
        measure
            .record(
                authenticated_peer,
                Authentication::Authenticated,
                MeasurementEvent::Connected,
            )
            .await,
        Ok(ApplyOutcome::Applied)
    );
    let runtime = lock_or_recover(&measure.state.runtime);
    assert_eq!(runtime.ledger.len(), 1);
    assert!(runtime.ledger.record(&authenticated_peer).is_some());
}
