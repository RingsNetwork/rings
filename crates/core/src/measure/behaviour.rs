use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [Measure].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement + Send + Sync>;

/// Type of Measure, see [crate::measure::Measure].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub type MeasureImpl = Arc<dyn BehaviourJudgement>;

use rings_measure::ApplyOutcome;
use rings_measure::MeasureError;

use super::Authentication;
use super::MeasureCounter;
use super::MeasurementBatch;
use super::MeasurementEvent;
use super::PeerMeasurement;
use super::PeerMeasurementPage;
use super::PeerQuality;

/// Runtime boundary for local peer-credit and reliability observations.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait Measure {
    /// Increment a legacy counter whose peer attribution was already established.
    ///
    /// New transport code should use [`Self::record`] so the authentication
    /// state remains explicit at the runtime boundary.
    async fn incr(&self, did: Did, counter: MeasureCounter);
    /// `get_count` returns the counter of the given peer.
    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64;

    /// Record one logical transport event with its explicit identity proof state.
    ///
    /// Implementations backed by [`rings_measure::MeasurementLedger`] should
    /// override this method so useful-byte credits are retained. The default is
    /// a compatibility bridge for counter-only test implementations: because
    /// they have no retained-peer set, they observe every proof-permitted local
    /// failure, while a ledger-backed override enforces known-peer retention.
    async fn record(
        &self,
        did: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(event) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        self.incr(did, MeasureCounter::from_event(event)).await;
        Ok(ApplyOutcome::Applied)
    }

    /// Record a homogeneous batch as one atomic logical transition.
    ///
    /// The provided compatibility implementation is deliberately non-atomic and
    /// projects only occurrence counts through [`Self::incr`]. Byte-aware or
    /// transactional implementations must override it to preserve aggregate
    /// useful bytes and all-or-nothing application.
    async fn record_batch(
        &self,
        did: Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> Result<ApplyOutcome, MeasureError> {
        if !authentication.permits(batch.event()) {
            return Ok(ApplyOutcome::IgnoredUnattributable);
        }
        let counter = MeasureCounter::from_event(batch.event());
        for _ in 0..batch.occurrences().get() {
            self.incr(did, counter).await;
        }
        Ok(ApplyOutcome::Applied)
    }

    /// Return the projected local measurement for one peer.
    ///
    /// Counter-only compatibility implementations have no complete policy or
    /// credit state and therefore return no projection by default.
    async fn peer_measurement(&self, _did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        Ok(None)
    }

    /// Return every retained local peer measurement.
    ///
    /// Counter-only compatibility implementations cannot enumerate their key
    /// space and therefore return an empty vector by default.
    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        Ok(Vec::new())
    }

    /// Return one bounded page after an exclusive DID cursor.
    ///
    /// Counter-only compatibility implementations cannot enumerate their key
    /// space and therefore return an empty page by default.
    async fn peer_measurements_page(
        &self,
        _after: Option<Did>,
        _limit: NonZeroUsize,
    ) -> Result<PeerMeasurementPage, MeasureError> {
        Ok(PeerMeasurementPage::default())
    }
}

/// `BehaviourJudgement` classifies local evidence about a peer.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait BehaviourJudgement: Measure {
    /// Classify local peer quality for DHT connection scheduling.
    ///
    /// This value is advisory. It orders connection attempts and does not gate
    /// Chord membership, routing, ownership, or storage placement.
    async fn quality(&self, did: Did) -> PeerQuality;
}
