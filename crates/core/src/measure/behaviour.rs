use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;

use crate::dht::Did;

/// Type of Measure, see [crate::measure::Measure].
pub type MeasureImpl = Arc<rings_runtime::maybe_send_sync!(dyn BehaviourJudgement)>;

use rings_measure::ApplyOutcome;
use rings_measure::EvidenceAdmissionReport;
use rings_measure::EvidenceCounters;
use rings_measure::EvidenceDigest;
use rings_measure::EvidenceError;
use rings_measure::EvidencePage;
use rings_measure::MeasureError;
use rings_measure::ProvisionalEvidenceRecord;

use super::Authentication;
use super::MeasurementBatch;
use super::MeasurementEvent;
use super::PeerMeasurement;
use super::PeerMeasurementPage;
use super::PeerQuality;

/// Runtime boundary for local peer-credit and reliability observations.
#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
pub trait Measure {
    /// Record one logical event with explicit peer attribution and useful bytes.
    /// Implementations must preserve the ledger's authentication and retention rules.
    async fn record(
        &self,
        did: Did,
        authentication: Authentication,
        event: MeasurementEvent,
    ) -> Result<ApplyOutcome, MeasureError>;

    /// Apply a homogeneous batch atomically, retaining occurrence and byte totals.
    /// Failure must not leave a partially applied batch.
    async fn record_batch(
        &self,
        did: Did,
        authentication: Authentication,
        batch: MeasurementBatch,
    ) -> Result<ApplyOutcome, MeasureError>;

    /// Return the projected local measurement for one peer.
    ///
    /// Observation-only implementations may omit query support.
    async fn peer_measurement(&self, _did: Did) -> Result<Option<PeerMeasurement>, MeasureError> {
        Ok(None)
    }

    /// Return every retained local peer measurement.
    ///
    /// Observation-only implementations return an empty vector by default.
    async fn peer_measurements(&self) -> Result<Vec<PeerMeasurement>, MeasureError> {
        Ok(Vec::new())
    }

    /// Return one bounded page after an exclusive DID cursor.
    ///
    /// Observation-only implementations return an empty page by default.
    async fn peer_measurements_page(
        &self,
        _after: Option<Did>,
        _limit: NonZeroUsize,
    ) -> Result<PeerMeasurementPage, MeasureError> {
        Ok(PeerMeasurementPage::default())
    }

    /// Admit one already-verified provisional receipt into bounded evidence storage.
    async fn admit_provisional_evidence(
        &self,
        _record: ProvisionalEvidenceRecord<Did>,
    ) -> Result<EvidenceAdmissionReport<Did>, EvidenceError> {
        Err(EvidenceError::StorageUnavailable)
    }

    /// Return one bounded page of provisional receipt evidence.
    async fn provisional_evidence_page(
        &self,
        _after: Option<EvidenceDigest>,
        _limit: NonZeroUsize,
    ) -> Result<EvidencePage<Did>, EvidenceError> {
        Err(EvidenceError::StorageUnavailable)
    }

    /// Return aggregate evidence counters without account-labelled dimensions.
    async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        EvidenceCounters::default()
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
