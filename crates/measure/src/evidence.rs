//! Bounded provisional service-receipt evidence.
//!
//! The store is deliberately generic over the account identifier. It owns only
//! canonical receipt bytes and independently bounded durable replay markers.
//! The protocol crate verifies receipt bytes before constructing a record and
//! again while restoring a snapshot.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::ops::Bound::Excluded;
use std::ops::Bound::Unbounded;

use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

use crate::UnixTime;

/// Current serialized provisional-evidence snapshot schema.
pub const EVIDENCE_SNAPSHOT_VERSION: u16 = 1;

/// Digest ordering key for one canonical receipt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EvidenceDigest([u8; 32]);

impl EvidenceDigest {
    /// Construct a digest from its canonical bytes.
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the digest bytes.
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Provider and beneficiary used for per-pair bounds.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EvidenceAccountPair<P> {
    /// Service provider account identifier.
    provider: P,
    /// Service beneficiary account identifier.
    beneficiary: P,
}

impl<P> EvidenceAccountPair<P> {
    /// Name an ordered provider-beneficiary pair.
    pub const fn new(provider: P, beneficiary: P) -> Self {
        Self {
            provider,
            beneficiary,
        }
    }

    /// Provider account.
    pub const fn provider(&self) -> &P {
        &self.provider
    }

    /// Beneficiary account.
    pub const fn beneficiary(&self) -> &P {
        &self.beneficiary
    }
}

/// Live-collection replay key specified by the provisional protocol.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct EvidenceFreshnessKey<P> {
    /// Overlay network identifier scoping this replay key.
    network_id: u32,
    /// Beneficiary account identifier bound to the receipt.
    beneficiary: P,
    /// Provisional epoch slot in which the receipt was issued.
    epoch_slot: u64,
    /// Unique 32-byte receipt nonce within `(network_id, beneficiary, epoch_slot)`.
    nonce: [u8; 32],
}

impl<P> EvidenceFreshnessKey<P> {
    /// Construct `(network, beneficiary, epoch, nonce)`.
    pub const fn new(network_id: u32, beneficiary: P, epoch_slot: u64, nonce: [u8; 32]) -> Self {
        Self {
            network_id,
            beneficiary,
            epoch_slot,
            nonce,
        }
    }

    /// Overlay identifier.
    pub const fn network_id(&self) -> u32 {
        self.network_id
    }

    /// Beneficiary account.
    pub const fn beneficiary(&self) -> &P {
        &self.beneficiary
    }

    /// Provisional epoch slot.
    pub const fn epoch_slot(&self) -> u64 {
        self.epoch_slot
    }

    /// Receipt nonce.
    pub const fn nonce(&self) -> [u8; 32] {
        self.nonce
    }
}

/// One already-verified provisional receipt in canonical wire form.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvisionalEvidenceRecord<P> {
    /// Ordered provider-beneficiary pair for per-pair capacity accounting.
    pair: EvidenceAccountPair<P>,
    /// Epoch-scoped replay key for duplicate and conflict detection.
    freshness: EvidenceFreshnessKey<P>,
    /// Canonical digest ordering key for this receipt.
    digest: EvidenceDigest,
    /// Canonical serialized receipt bytes.
    #[serde(with = "canonical_bytes")]
    canonical_receipt: Vec<u8>,
    /// Local observation timestamp used for eviction ordering.
    observed_at: UnixTime,
    /// Monotonic replay floor active when this receipt was observed.
    replay_floor: u64,
}

/// Serde adapter encoding canonical receipt bytes as raw byte slices while accepting sequences.
mod canonical_bytes {
    use serde::de;
    use serde::Deserializer;
    use serde::Serializer;

    /// Serializes `bytes` as a serde byte slice.
    pub(super) fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        serializer.serialize_bytes(bytes)
    }

    /// Deserializes canonical receipt bytes from either a byte slice or an integer sequence.
    pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where D: Deserializer<'de> {
        struct BytesOrSeqVisitor;

        impl<'de> de::Visitor<'de> for BytesOrSeqVisitor {
            type Value = Vec<u8>;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("canonical receipt byte slice or byte sequence")
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E> {
                Ok(value.to_vec())
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E> {
                Ok(value)
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where A: de::SeqAccess<'de> {
                let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
                while let Some(byte) = seq.next_element()? {
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }

        deserializer.deserialize_byte_buf(BytesOrSeqVisitor)
    }
}

impl<P> ProvisionalEvidenceRecord<P> {
    /// Construct a verified evidence record at the runtime boundary.
    pub fn new(
        pair: EvidenceAccountPair<P>,
        freshness: EvidenceFreshnessKey<P>,
        digest: EvidenceDigest,
        canonical_receipt: Vec<u8>,
        observed_at: UnixTime,
        replay_floor: u64,
    ) -> Self {
        Self {
            pair,
            freshness,
            digest,
            canonical_receipt,
            observed_at,
            replay_floor,
        }
    }

    /// Ordered provider-beneficiary pair.
    pub const fn pair(&self) -> &EvidenceAccountPair<P> {
        &self.pair
    }

    /// Epoch-scoped replay key.
    pub const fn freshness(&self) -> &EvidenceFreshnessKey<P> {
        &self.freshness
    }

    /// Canonical receipt digest.
    pub const fn digest(&self) -> EvidenceDigest {
        self.digest
    }

    /// Canonical receipt bytes.
    pub fn canonical_receipt(&self) -> &[u8] {
        &self.canonical_receipt
    }

    /// Local observation time.
    pub const fn observed_at(&self) -> UnixTime {
        self.observed_at
    }

    /// Oldest provisional epoch that remained admissible at observation.
    pub const fn replay_floor(&self) -> u64 {
        self.replay_floor
    }

    /// Returns the length in bytes of the canonical receipt payload.
    fn byte_len(&self) -> usize {
        self.canonical_receipt.len()
    }
}

/// Durable replay marker retained independently from evictable receipt bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceReplayMarker<P> {
    /// Ordered provider-beneficiary pair associated with the admitted receipt.
    pair: EvidenceAccountPair<P>,
    /// Epoch-scoped freshness key occupied by this marker.
    freshness: EvidenceFreshnessKey<P>,
    /// Canonical receipt digest bound to `freshness`.
    digest: EvidenceDigest,
}

impl<P> EvidenceReplayMarker<P> {
    /// Extracts a durable replay marker from a verified evidence record.
    fn from_record(record: &ProvisionalEvidenceRecord<P>) -> Self
    where P: Clone {
        Self {
            pair: record.pair.clone(),
            freshness: record.freshness.clone(),
            digest: record.digest,
        }
    }

    /// Ordered provider-beneficiary pair that admitted the key.
    pub const fn pair(&self) -> &EvidenceAccountPair<P> {
        &self.pair
    }

    /// Epoch-scoped nonce key that has already been admitted.
    pub const fn freshness(&self) -> &EvidenceFreshnessKey<P> {
        &self.freshness
    }

    /// Digest admitted for the freshness key.
    pub const fn digest(&self) -> EvidenceDigest {
        self.digest
    }
}

/// Hard resident-state limits for provisional evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvidenceLimits {
    /// Maximum records retained globally.
    pub max_records: NonZeroUsize,
    /// Maximum canonical receipt bytes retained globally.
    pub max_bytes: NonZeroUsize,
    /// Maximum records retained for one ordered account pair.
    pub max_records_per_pair: NonZeroUsize,
    /// Maximum canonical receipt bytes retained for one ordered account pair.
    pub max_bytes_per_pair: NonZeroUsize,
    /// Maximum durable replay markers retained globally.
    pub max_replay_markers: NonZeroUsize,
    /// Maximum durable replay markers retained for one beneficiary.
    pub max_replay_markers_per_beneficiary: NonZeroUsize,
}

impl Default for EvidenceLimits {
    fn default() -> Self {
        Self {
            max_records: nonzero(4_096),
            max_bytes: nonzero(16 * 1024 * 1024),
            max_records_per_pair: nonzero(64),
            max_bytes_per_pair: nonzero(1024 * 1024),
            max_replay_markers: nonzero(16_384),
            max_replay_markers_per_beneficiary: nonzero(256),
        }
    }
}

/// Constructs a `NonZeroUsize` from a known non-zero compile-time constant.
#[allow(
    clippy::unwrap_used,
    reason = "the non-zero default literals are validated during const evaluation"
)]
const fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

/// Aggregate evidence-store counters. Account identifiers are never labels.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceCounters {
    /// Total receipts newly admitted into the store.
    admitted: u64,
    /// Total exact duplicate receipts rejected.
    duplicates: u64,
    /// Total freshness-key conflicts rejected.
    conflicts: u64,
    /// Total retained records evicted to enforce bounds.
    evicted_records: u64,
    /// Total canonical receipt bytes freed by eviction.
    evicted_bytes: u64,
    /// Total records rejected as invalid, oversized, or stale.
    rejected_records: u64,
    /// Total valid receipts rejected because replay marker capacity was full.
    replay_capacity_rejections: u64,
    /// Total persisted replay markers rejected during restore.
    rejected_replay_markers: u64,
}

impl EvidenceCounters {
    /// Newly admitted receipts.
    pub const fn admitted(self) -> u64 {
        self.admitted
    }

    /// Exact duplicate receipts rejected, including evicted receipt bytes.
    pub const fn duplicates(self) -> u64 {
        self.duplicates
    }

    /// Freshness-key conflicts rejected.
    pub const fn conflicts(self) -> u64 {
        self.conflicts
    }

    /// Records evicted to preserve a bound.
    pub const fn evicted_records(self) -> u64 {
        self.evicted_records
    }

    /// Canonical receipt bytes removed by eviction.
    pub const fn evicted_bytes(self) -> u64 {
        self.evicted_bytes
    }

    /// Oversized or invalid persisted records rejected.
    pub const fn rejected_records(self) -> u64 {
        self.rejected_records
    }

    /// Valid receipts rejected because replay memory was full.
    pub const fn replay_capacity_rejections(self) -> u64 {
        self.replay_capacity_rejections
    }

    /// Invalid persisted replay markers rejected during restore.
    pub const fn rejected_replay_markers(self) -> u64 {
        self.rejected_replay_markers
    }
}

/// Admission outcome for a candidate receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceAdmission {
    /// The candidate was admitted, possibly evicting older evidence.
    Admitted,
    /// The same canonical digest already has a durable replay marker.
    Duplicate,
    /// The freshness key was already occupied by a different admitted digest.
    Conflict {
        /// Digest recorded for the freshness key.
        retained: EvidenceDigest,
    },
    /// The receipt was not admitted because durable replay memory was full.
    ReplayCapacityExhausted,
}

/// One deterministically evicted evidence record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceEviction<P> {
    /// Provider-beneficiary pair whose record was evicted.
    pair: EvidenceAccountPair<P>,
    /// Canonical digest of the evicted receipt.
    digest: EvidenceDigest,
    /// Number of canonical receipt bytes released.
    bytes: usize,
}

impl<P> EvidenceEviction<P> {
    /// Account pair whose evidence was evicted.
    pub const fn pair(&self) -> &EvidenceAccountPair<P> {
        &self.pair
    }

    /// Digest of the evicted receipt.
    pub const fn digest(&self) -> EvidenceDigest {
        self.digest
    }

    /// Canonical receipt bytes released.
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Result of one bounded admission transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceAdmissionReport<P> {
    /// Admission outcome for the candidate receipt.
    admission: EvidenceAdmission,
    /// Older records evicted to maintain configured bounds.
    evictions: Vec<EvidenceEviction<P>>,
}

impl<P> EvidenceAdmissionReport<P> {
    /// Admission decision.
    pub const fn admission(&self) -> EvidenceAdmission {
        self.admission
    }

    /// Records removed while admitting the candidate.
    pub fn evictions(&self) -> &[EvidenceEviction<P>] {
        &self.evictions
    }
}

/// Versioned persistence form for the bounded evidence store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceSnapshot<P> {
    /// Schema version.
    pub schema_version: u16,
    /// Canonical records in digest order.
    pub records: Vec<ProvisionalEvidenceRecord<P>>,
    /// Durable markers for admitted keys, including evicted receipts.
    pub replay_markers: Vec<EvidenceReplayMarker<P>>,
    /// Monotonic lower bound below which regressed-clock admissions fail closed.
    pub replay_floor: u64,
    /// Aggregate counters without account labels.
    pub counters: EvidenceCounters,
}

/// One durably persisted incremental provisional-evidence admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EvidenceAdmissionDelta<P> {
    /// Schema version (`EVIDENCE_SNAPSHOT_VERSION`).
    pub schema_version: u16,
    /// Strictly monotonic sequence number within one node's evidence journal.
    pub sequence: u64,
    /// Admitted provisional evidence record.
    pub record: ProvisionalEvidenceRecord<P>,
}

/// Versioned storage value stored under keys of `provisional-evidence`.
///
/// Steady-state admissions append O(1) [`Self::Delta`] (or [`Self::ReplayFloor`]) entries so
/// admission durability never rewrites the full [`EvidenceSnapshot`]. Periodic compaction and
/// clean flushes fold accumulated deltas into a single [`Self::Snapshot`] entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum EvidenceStorageEntry<P> {
    /// Base checkpoint covering all deltas with `sequence <= compacted_through_sequence`.
    Snapshot {
        /// Greatest delta sequence included in `snapshot` (`0` before any delta existed).
        compacted_through_sequence: u64,
        /// Serialized evidence snapshot at this checkpoint.
        snapshot: EvidenceSnapshot<P>,
    },
    /// Single admitted evidence record written after the latest compacted checkpoint.
    Delta(EvidenceAdmissionDelta<P>),
    /// Replay-floor advance emitted when a rejected admission raised `replay_floor`.
    ReplayFloor {
        /// Schema version (`EVIDENCE_SNAPSHOT_VERSION`).
        schema_version: u16,
        /// Strictly monotonic sequence number within one node's evidence journal.
        sequence: u64,
        /// Monotonic replay floor reached after the transition.
        replay_floor: u64,
    },
}

/// Summary of invalid records/markers skipped and valid records evicted during restore.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EvidenceLoadReport {
    /// Number of persisted records rejected during restore.
    rejected_records: usize,
    /// Number of valid records evicted to enforce active limits.
    evicted_records: usize,
    /// Number of persisted replay markers rejected during restore.
    rejected_replay_markers: usize,
}

impl EvidenceLoadReport {
    /// Snapshot entries rejected by structural or protocol validation.
    pub const fn rejected_records(self) -> usize {
        self.rejected_records
    }

    /// Valid snapshot entries deterministically evicted to restore active bounds.
    pub const fn evicted_records(self) -> usize {
        self.evicted_records
    }

    /// Replay markers rejected by runtime binding or structural validation.
    pub const fn rejected_replay_markers(self) -> usize {
        self.rejected_replay_markers
    }

    /// Accumulate the counts of `other` into `self`.
    pub fn merge(&mut self, other: Self) {
        self.rejected_records = self.rejected_records.saturating_add(other.rejected_records);
        self.evicted_records = self.evicted_records.saturating_add(other.evicted_records);
        self.rejected_replay_markers = self
            .rejected_replay_markers
            .saturating_add(other.rejected_replay_markers);
    }
}

/// One bounded page in canonical digest order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidencePage<P> {
    /// Evidence records returned in this page.
    records: Vec<ProvisionalEvidenceRecord<P>>,
    /// Exclusive continuation digest if more records remain.
    next_cursor: Option<EvidenceDigest>,
}

impl<P> EvidencePage<P> {
    /// Records in this bounded page.
    pub fn records(&self) -> &[ProvisionalEvidenceRecord<P>] {
        &self.records
    }

    /// Exclusive digest cursor for the next page.
    pub const fn next_cursor(&self) -> Option<EvidenceDigest> {
        self.next_cursor
    }

    /// Consume the page into records and continuation cursor.
    pub fn into_parts(self) -> (Vec<ProvisionalEvidenceRecord<P>>, Option<EvidenceDigest>) {
        (self.records, self.next_cursor)
    }
}

/// Failure to represent a bounded evidence transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum EvidenceError {
    /// Runtime implementation has no provisional-evidence component.
    #[error("provisional evidence storage is unavailable")]
    StorageUnavailable,
    /// A snapshot uses an unsupported schema.
    #[error("unsupported evidence snapshot version {found}")]
    UnsupportedSnapshotVersion {
        /// Unrecognized version.
        found: u16,
    },
    /// The record's `pair.beneficiary` differs from `freshness.beneficiary`.
    #[error("evidence pair beneficiary does not match freshness beneficiary")]
    BeneficiaryMismatch,
    /// A canonical receipt is empty.
    #[error("canonical receipt bytes are empty")]
    EmptyReceipt,
    /// One receipt cannot fit the configured per-pair or global byte bound.
    #[error("receipt has {bytes} bytes, exceeding evidence limit {limit}")]
    ReceiptTooLarge {
        /// Candidate receipt bytes.
        bytes: usize,
        /// Effective maximum bytes for one record.
        limit: usize,
    },
    /// The candidate belongs to an epoch below the monotonic replay floor.
    #[error("receipt epoch {epoch} is below replay floor {floor}")]
    FreshnessBeforeReplayFloor {
        /// Candidate receipt epoch.
        epoch: u64,
        /// Oldest epoch that may still be admitted.
        floor: u64,
    },
    /// A persisted replay set exceeds the configured hard bound.
    #[error("persisted replay markers {markers} exceed limit {limit}")]
    ReplayMarkerLimitExceeded {
        /// Persisted marker count in the rejected scope.
        markers: usize,
        /// Configured hard limit.
        limit: usize,
    },
    /// Durable evidence persistence failed; no admission was committed.
    #[error("durable provisional evidence persistence failed")]
    PersistenceUnavailable,
}

/// Pure bounded provisional-evidence state.
#[derive(Clone)]
pub struct ProvisionalEvidenceStore<P> {
    /// Hard global and per-pair resident-state bounds.
    limits: EvidenceLimits,
    /// Retained canonical receipts keyed by digest.
    records: BTreeMap<EvidenceDigest, ProvisionalEvidenceRecord<P>>,
    /// Durable replay markers keyed by epoch-scoped freshness key.
    freshness: BTreeMap<EvidenceFreshnessKey<P>, EvidenceReplayMarker<P>>,
    /// Monotonic minimum admissible epoch slot.
    replay_floor: u64,
    /// Total canonical receipt bytes currently resident in `records`.
    resident_bytes: usize,
    /// Aggregate admission, eviction, and rejection counters.
    counters: EvidenceCounters,
}

impl<P> ProvisionalEvidenceStore<P>
where P: Clone + Ord
{
    /// Construct an empty store under explicit limits.
    pub fn new(limits: EvidenceLimits) -> Self {
        Self {
            limits,
            records: BTreeMap::new(),
            freshness: BTreeMap::new(),
            replay_floor: 0,
            resident_bytes: 0,
            counters: EvidenceCounters::default(),
        }
    }

    /// Restore valid records and durable replay markers independently.
    pub fn from_snapshot_with_validator(
        snapshot: EvidenceSnapshot<P>,
        limits: EvidenceLimits,
        validator: impl Fn(&ProvisionalEvidenceRecord<P>) -> bool,
        replay_marker_validator: impl Fn(&EvidenceReplayMarker<P>) -> bool,
    ) -> Result<(Self, EvidenceLoadReport), EvidenceError> {
        if snapshot.schema_version != EVIDENCE_SNAPSHOT_VERSION {
            return Err(EvidenceError::UnsupportedSnapshotVersion {
                found: snapshot.schema_version,
            });
        }
        let prior_counters = snapshot.counters;
        let mut records = snapshot.records;
        let mut replay_markers = snapshot.replay_markers;
        records.sort_by_key(|record| (record.observed_at(), record.digest()));
        let mut store = Self::new(limits);
        store.replay_floor = snapshot.replay_floor;
        let mut report = EvidenceLoadReport::default();
        for record in records {
            if !validator(&record) || store.validate_size(&record).is_err() {
                report.rejected_records = report.rejected_records.saturating_add(1);
                continue;
            }
            let marker = EvidenceReplayMarker::from_record(&record);
            if !store.can_restore_marker(&marker) {
                report.rejected_records = report.rejected_records.saturating_add(1);
                continue;
            }
            store.ensure_replay_capacity(&marker)?;
            store.freshness.insert(marker.freshness.clone(), marker);
            let pair = record.pair.clone();
            let digest = record.digest;
            store.resident_bytes = store.resident_bytes.saturating_add(record.byte_len());
            store.records.insert(digest, record);
            let evictions = store.evict_to_bounds(&pair, digest);
            report.evicted_records = report.evicted_records.saturating_add(evictions.len());
        }
        replay_markers.sort_by(|left, right| {
            (&left.freshness, left.digest, &left.pair).cmp(&(
                &right.freshness,
                right.digest,
                &right.pair,
            ))
        });
        for marker in replay_markers {
            if !store.marker_is_structural(&marker) || !replay_marker_validator(&marker) {
                report.rejected_replay_markers = report.rejected_replay_markers.saturating_add(1);
                continue;
            }
            if marker.freshness.epoch_slot < store.replay_floor
                && !store.records.contains_key(&marker.digest)
            {
                continue;
            }
            if let Some(existing) = store.freshness.get(&marker.freshness) {
                if existing == &marker {
                    continue;
                }
                report.rejected_replay_markers = report.rejected_replay_markers.saturating_add(1);
                continue;
            }
            if store
                .freshness
                .values()
                .any(|existing| existing.digest == marker.digest)
            {
                report.rejected_replay_markers = report.rejected_replay_markers.saturating_add(1);
                continue;
            }
            store.ensure_replay_capacity(&marker)?;
            store.freshness.insert(marker.freshness.clone(), marker);
        }
        let restore_counters = store.counters;
        store.counters = EvidenceCounters {
            admitted: prior_counters.admitted,
            duplicates: prior_counters.duplicates,
            conflicts: prior_counters.conflicts,
            evicted_records: prior_counters
                .evicted_records
                .saturating_add(restore_counters.evicted_records),
            evicted_bytes: prior_counters
                .evicted_bytes
                .saturating_add(restore_counters.evicted_bytes),
            rejected_records: prior_counters
                .rejected_records
                .saturating_add(u64::try_from(report.rejected_records).unwrap_or(u64::MAX)),
            replay_capacity_rejections: prior_counters.replay_capacity_rejections,
            rejected_replay_markers: prior_counters
                .rejected_replay_markers
                .saturating_add(u64::try_from(report.rejected_replay_markers).unwrap_or(u64::MAX)),
        };
        Ok((store, report))
    }

    /// Apply one admission transition.
    pub fn admit(
        &mut self,
        record: ProvisionalEvidenceRecord<P>,
    ) -> Result<EvidenceAdmissionReport<P>, EvidenceError> {
        if record.pair.beneficiary() != record.freshness.beneficiary() {
            self.counters.rejected_records = self.counters.rejected_records.saturating_add(1);
            return Err(EvidenceError::BeneficiaryMismatch);
        }
        self.validate_size(&record)?;
        self.advance_replay_floor(record.replay_floor);
        if record.freshness.epoch_slot < self.replay_floor {
            self.counters.rejected_records = self.counters.rejected_records.saturating_add(1);
            return Err(EvidenceError::FreshnessBeforeReplayFloor {
                epoch: record.freshness.epoch_slot,
                floor: self.replay_floor,
            });
        }
        if self
            .freshness
            .values()
            .any(|marker| marker.digest == record.digest)
        {
            self.counters.duplicates = self.counters.duplicates.saturating_add(1);
            return Ok(EvidenceAdmissionReport {
                admission: EvidenceAdmission::Duplicate,
                evictions: Vec::new(),
            });
        }
        if let Some(retained) = self.freshness.get(&record.freshness) {
            self.counters.conflicts = self.counters.conflicts.saturating_add(1);
            return Ok(EvidenceAdmissionReport {
                admission: EvidenceAdmission::Conflict {
                    retained: retained.digest,
                },
                evictions: Vec::new(),
            });
        }

        let marker = EvidenceReplayMarker::from_record(&record);
        if self.ensure_replay_capacity(&marker).is_err() {
            self.counters.replay_capacity_rejections =
                self.counters.replay_capacity_rejections.saturating_add(1);
            return Ok(EvidenceAdmissionReport {
                admission: EvidenceAdmission::ReplayCapacityExhausted,
                evictions: Vec::new(),
            });
        }
        let pair = record.pair.clone();
        let digest = record.digest;
        self.resident_bytes = self.resident_bytes.saturating_add(record.byte_len());
        self.freshness.insert(record.freshness.clone(), marker);
        self.records.insert(digest, record);
        self.counters.admitted = self.counters.admitted.saturating_add(1);

        let evictions = self.evict_to_bounds(&pair, digest);
        Ok(EvidenceAdmissionReport {
            admission: EvidenceAdmission::Admitted,
            evictions,
        })
    }

    /// Return one bounded page after an exclusive digest cursor.
    pub fn page(&self, after: Option<EvidenceDigest>, limit: NonZeroUsize) -> EvidencePage<P> {
        let mut records = match after {
            Some(digest) => self.records.range((Excluded(digest), Unbounded)),
            None => self.records.range(..),
        };
        let mut page = Vec::with_capacity(limit.get().min(self.records.len()));
        let mut last = None;
        for _ in 0..limit.get() {
            let Some((digest, record)) = records.next() else {
                break;
            };
            last = Some(*digest);
            page.push(record.clone());
        }
        let next_cursor = records.next().is_some().then_some(last).flatten();
        EvidencePage {
            records: page,
            next_cursor,
        }
    }

    /// Aggregate counters with no account-labelled dimension.
    pub const fn counters(&self) -> EvidenceCounters {
        self.counters
    }

    /// Resident record count.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the store retains no evidence.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Resident canonical receipt bytes.
    pub const fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Durable replay-marker count, including markers whose receipt was evicted.
    pub fn replay_marker_len(&self) -> usize {
        self.freshness.len()
    }

    /// Monotonic oldest epoch that may still be admitted after clock regression.
    pub const fn replay_floor(&self) -> u64 {
        self.replay_floor
    }

    /// Advance the monotonic replay floor when replaying a persisted `ReplayFloor` checkpoint.
    pub fn restore_replay_floor(&mut self, floor: u64) {
        self.advance_replay_floor(floor);
    }

    /// Replay one persisted [`EvidenceAdmissionDelta`] during startup restore.
    ///
    /// Returns [`EvidenceError::UnsupportedSnapshotVersion`] on an unrecognized schema version.
    /// Invalid or corrupt records are isolated and skipped while preserving valid neighbors,
    /// matching [`Self::from_snapshot_with_validator`].
    pub fn replay_persisted_delta(
        &mut self,
        delta: EvidenceAdmissionDelta<P>,
        validator: impl FnOnce(&ProvisionalEvidenceRecord<P>) -> bool,
    ) -> Result<EvidenceLoadReport, EvidenceError> {
        if delta.schema_version != EVIDENCE_SNAPSHOT_VERSION {
            return Err(EvidenceError::UnsupportedSnapshotVersion {
                found: delta.schema_version,
            });
        }
        if !validator(&delta.record) {
            self.counters.rejected_records = self.counters.rejected_records.saturating_add(1);
            self.counters.rejected_replay_markers =
                self.counters.rejected_replay_markers.saturating_add(1);
            return Ok(EvidenceLoadReport {
                rejected_records: 1,
                evicted_records: 0,
                rejected_replay_markers: 1,
            });
        }
        match self.admit(delta.record) {
            Ok(report) => Ok(EvidenceLoadReport {
                rejected_records: 0,
                evicted_records: report.evictions().len(),
                rejected_replay_markers: 0,
            }),
            Err(_) => {
                // `self.admit` already incremented `self.counters.rejected_records`.
                self.counters.rejected_replay_markers =
                    self.counters.rejected_replay_markers.saturating_add(1);
                Ok(EvidenceLoadReport {
                    rejected_records: 1,
                    evicted_records: 0,
                    rejected_replay_markers: 1,
                })
            }
        }
    }

    /// Produce the deterministic persistence form.
    pub fn snapshot(&self) -> EvidenceSnapshot<P> {
        EvidenceSnapshot {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            records: self.records.values().cloned().collect(),
            replay_markers: self.freshness.values().cloned().collect(),
            replay_floor: self.replay_floor,
            counters: self.counters,
        }
    }

    /// Raises `replay_floor` monotonically and prunes expired replay markers with no resident record.
    fn advance_replay_floor(&mut self, floor: u64) {
        self.replay_floor = self.replay_floor.max(floor);
        let records = &self.records;
        self.freshness.retain(|_, marker| {
            marker.freshness.epoch_slot >= self.replay_floor || records.contains_key(&marker.digest)
        });
    }

    /// Returns whether `marker`'s pair beneficiary matches its freshness-key beneficiary.
    fn marker_is_structural(&self, marker: &EvidenceReplayMarker<P>) -> bool {
        marker.pair.beneficiary() == marker.freshness.beneficiary()
    }

    /// Returns whether `marker` is structurally valid and does not conflict with restored markers.
    fn can_restore_marker(&self, marker: &EvidenceReplayMarker<P>) -> bool {
        self.marker_is_structural(marker)
            && !self.freshness.contains_key(&marker.freshness)
            && !self
                .freshness
                .values()
                .any(|existing| existing.digest == marker.digest && existing != marker)
    }

    /// Checks that inserting `marker` stays within global and per-beneficiary replay marker limits.
    fn ensure_replay_capacity(
        &self,
        marker: &EvidenceReplayMarker<P>,
    ) -> Result<(), EvidenceError> {
        let next_global = self.freshness.len().saturating_add(1);
        if next_global > self.limits.max_replay_markers.get() {
            return Err(EvidenceError::ReplayMarkerLimitExceeded {
                markers: next_global,
                limit: self.limits.max_replay_markers.get(),
            });
        }
        let beneficiary_markers = self
            .freshness
            .values()
            .filter(|existing| existing.freshness.beneficiary() == marker.freshness.beneficiary())
            .count()
            .saturating_add(1);
        if beneficiary_markers > self.limits.max_replay_markers_per_beneficiary.get() {
            return Err(EvidenceError::ReplayMarkerLimitExceeded {
                markers: beneficiary_markers,
                limit: self.limits.max_replay_markers_per_beneficiary.get(),
            });
        }
        Ok(())
    }

    /// Evicts oldest records for `pair` and then globally until all resident bounds hold.
    fn evict_to_bounds(
        &mut self,
        pair: &EvidenceAccountPair<P>,
        candidate: EvidenceDigest,
    ) -> Vec<EvidenceEviction<P>> {
        let mut evictions = Vec::new();
        while self.pair_over_bound(pair) {
            let Some(victim) = self.oldest_digest_excluding(Some(pair), candidate) else {
                break;
            };
            if let Some(eviction) = self.evict(victim) {
                evictions.push(eviction);
            }
        }
        while self.global_over_bound() {
            let Some(victim) = self.oldest_digest_excluding(None, candidate) else {
                break;
            };
            if let Some(eviction) = self.evict(victim) {
                evictions.push(eviction);
            }
        }
        evictions
    }

    /// Validates that `record` is non-empty and fits within both global and per-pair byte limits.
    fn validate_size(
        &mut self,
        record: &ProvisionalEvidenceRecord<P>,
    ) -> Result<(), EvidenceError> {
        let bytes = record.byte_len();
        if bytes == 0 {
            self.counters.rejected_records = self.counters.rejected_records.saturating_add(1);
            return Err(EvidenceError::EmptyReceipt);
        }
        let limit = self
            .limits
            .max_bytes
            .get()
            .min(self.limits.max_bytes_per_pair.get());
        if bytes > limit {
            self.counters.rejected_records = self.counters.rejected_records.saturating_add(1);
            return Err(EvidenceError::ReceiptTooLarge { bytes, limit });
        }
        Ok(())
    }

    /// Returns whether resident records or bytes for `pair` exceed per-pair limits.
    fn pair_over_bound(&self, pair: &EvidenceAccountPair<P>) -> bool {
        let (records, bytes) = self.pair_usage(pair);
        records > self.limits.max_records_per_pair.get()
            || bytes > self.limits.max_bytes_per_pair.get()
    }

    /// Returns whether total resident records or bytes exceed global limits.
    fn global_over_bound(&self) -> bool {
        self.records.len() > self.limits.max_records.get()
            || self.resident_bytes > self.limits.max_bytes.get()
    }

    /// Counts resident `(records, bytes)` currently held for `pair`.
    fn pair_usage(&self, pair: &EvidenceAccountPair<P>) -> (usize, usize) {
        self.records
            .values()
            .filter(|record| &record.pair == pair)
            .fold((0_usize, 0_usize), |(records, bytes), record| {
                (
                    records.saturating_add(1),
                    bytes.saturating_add(record.byte_len()),
                )
            })
    }

    /// Finds the oldest resident digest (breaking ties by digest), excluding `excluded`.
    fn oldest_digest_excluding(
        &self,
        pair: Option<&EvidenceAccountPair<P>>,
        excluded: EvidenceDigest,
    ) -> Option<EvidenceDigest> {
        self.records
            .iter()
            .filter(|(digest, record)| {
                **digest != excluded && pair.is_none_or(|pair| &record.pair == pair)
            })
            .min_by_key(|(digest, record)| (record.observed_at, **digest))
            .map(|(digest, _)| *digest)
    }

    /// Removes `digest` from resident records and updates byte and eviction counters.
    fn evict(&mut self, digest: EvidenceDigest) -> Option<EvidenceEviction<P>> {
        let record = self.records.remove(&digest)?;
        let bytes = record.byte_len();
        self.resident_bytes = self.resident_bytes.saturating_sub(bytes);
        self.counters.evicted_records = self.counters.evicted_records.saturating_add(1);
        self.counters.evicted_bytes = self
            .counters
            .evicted_bytes
            .saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        Some(EvidenceEviction {
            pair: record.pair,
            digest,
            bytes,
        })
    }
}

#[cfg(test)]
mod tests;

// The bounded native model checks every ordering of the finite admission,
// conflict, eviction, rejection, clock, and crash-restart schedule used for refinement.
#[cfg(all(test, not(target_family = "wasm")))]
mod model;
