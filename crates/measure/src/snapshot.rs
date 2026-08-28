use serde::Deserialize;
use serde::Serialize;

use crate::PeerRecord;

/// Current serialized measurement snapshot schema version.
pub const MEASUREMENT_SNAPSHOT_VERSION: u16 = 1;

/// One peer entry in a versioned measurement snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord<P> {
    /// Stable peer key supplied by the integrating runtime.
    pub peer: P,
    /// Pure local state associated with the peer.
    pub record: PeerRecord,
}

/// Versioned, deterministic serialization boundary for a measurement ledger.
///
/// Snapshot fields are intentionally public because persisted data is untrusted
/// input. [`crate::MeasurementLedger::from_snapshot`] validates the version and
/// duplicate-key invariant before constructing live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementSnapshot<P> {
    /// Schema version used to encode `records`.
    pub schema_version: u16,
    /// Peer records in stable key order when produced by the ledger.
    pub records: Vec<SnapshotRecord<P>>,
}
