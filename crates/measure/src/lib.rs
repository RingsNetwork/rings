#![deny(missing_docs)]

//! Pure local peer measurement algorithms.
//!
//! This crate models two independent local relations:
//!
//! - [`CreditRecord`] records useful bytes exchanged with an authenticated peer
//!   and computes an aMule-compatible [`CreditScore`].
//! - [`ReliabilityWindow`] records recent logical transport outcomes and derives
//!   a [`ReliabilityClass`] used only for advisory ordering.
//!
//! Time, storage, networking, tasks, locks, and process lifecycle are effects.
//! Callers pass timestamps and explicitly attributable events into the pure
//! [`MeasurementLedger`] transition and persist [`MeasurementSnapshot`] values
//! outside this crate.

mod credit;
mod error;
mod event;
mod evidence;
mod ledger;
mod reliability;
mod snapshot;
mod time;

pub use credit::CreditPolicy;
pub use credit::CreditRecord;
pub use credit::CreditScore;
pub use error::MeasureError;
pub use error::PolicyError;
pub use event::ApplyOutcome;
pub use event::Authentication;
pub use event::MeasurementBatch;
pub use event::MeasurementEvent;
pub use event::Metric;
pub use evidence::EvidenceAccountPair;
pub use evidence::EvidenceAdmission;
pub use evidence::EvidenceAdmissionReport;
pub use evidence::EvidenceCounters;
pub use evidence::EvidenceDigest;
pub use evidence::EvidenceError;
pub use evidence::EvidenceEviction;
pub use evidence::EvidenceFreshnessKey;
pub use evidence::EvidenceLimits;
pub use evidence::EvidenceLoadReport;
pub use evidence::EvidencePage;
pub use evidence::EvidenceSnapshot;
pub use evidence::ProvisionalEvidenceRecord;
pub use evidence::ProvisionalEvidenceStore;
pub use evidence::EVIDENCE_SNAPSHOT_VERSION;
pub use ledger::ApplyReport;
pub use ledger::LedgerReconciliation;
pub use ledger::MeasurementLedger;
pub use ledger::MeasurementPage;
pub use ledger::MeasurementProjection;
pub use ledger::PeerMeasurement;
pub use ledger::PeerMeasurementFailure;
pub use ledger::PeerRecord;
pub use ledger::PruneReport;
pub use ledger::DEFAULT_MAX_RETAINED_PEERS;
pub use reliability::order_peers_by_reliability;
pub use reliability::ReliabilityClass;
pub use reliability::ReliabilityEvidence;
pub use reliability::ReliabilityPolicy;
pub use reliability::ReliabilityThresholds;
pub use reliability::ReliabilityWindow;
pub use snapshot::MeasurementSnapshot;
pub use snapshot::SnapshotRecord;
pub use snapshot::MEASUREMENT_SNAPSHOT_VERSION;
pub use time::UnixTime;
