use thiserror::Error;

use crate::Metric;
use crate::UnixTime;

/// Invalid measurement policy configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PolicyError {
    /// A reliability window must contain at least one second.
    #[error("reliability window must be non-zero")]
    ZeroReliabilityWindow,
    /// Credit retention must contain at least one second.
    #[error("credit retention must be non-zero")]
    ZeroCreditRetention,
}

/// Failure of a pure measurement state transition or snapshot validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MeasureError {
    /// A checked counter update exceeded `u64::MAX`.
    #[error("measurement counter overflow: {metric:?}")]
    CounterOverflow {
        /// Counter that could not represent the next value.
        metric: Metric,
    },
    /// An event or query moved behind the current record time.
    #[error("measurement clock regressed from {current:?} to {observed:?}")]
    ClockRegression {
        /// Timestamp supplied by the adapter.
        observed: UnixTime,
        /// Latest timestamp or aligned epoch already represented by the record.
        current: UnixTime,
    },
    /// A snapshot uses a schema version this crate cannot interpret.
    #[error("unsupported measurement snapshot version {found}")]
    UnsupportedSnapshotVersion {
        /// Unrecognized schema version.
        found: u16,
    },
    /// A snapshot contains more than one record for the same peer key.
    #[error("measurement snapshot contains a duplicate peer")]
    DuplicatePeerInSnapshot,
    /// A snapshot stores reliability evidence without a timestamped epoch.
    #[error("measurement snapshot has reliability evidence without an epoch")]
    SnapshotEvidenceWithoutEpoch,
    /// A snapshot stores an epoch even though its reliability bucket is empty.
    #[error("measurement snapshot has an empty timestamped reliability epoch")]
    SnapshotEpochWithoutEvidence,
    /// A snapshot reliability epoch begins after the record's latest observation.
    #[error("measurement snapshot reliability epoch is after last_seen")]
    SnapshotEpochAfterLastSeen,
}
