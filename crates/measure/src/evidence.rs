//! Bounded provisional service-receipt evidence.
//!
//! The store is deliberately generic over the account identifier. It owns only
//! canonical receipt bytes and the metadata needed for bounded admission. The
//! protocol crate verifies those bytes before constructing a record and again
//! while restoring a snapshot.

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
    provider: P,
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
    network_id: u32,
    beneficiary: P,
    epoch_slot: u64,
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
    pair: EvidenceAccountPair<P>,
    freshness: EvidenceFreshnessKey<P>,
    digest: EvidenceDigest,
    canonical_receipt: Vec<u8>,
    observed_at: UnixTime,
}

impl<P> ProvisionalEvidenceRecord<P> {
    /// Construct a verified evidence record at the runtime boundary.
    pub fn new(
        pair: EvidenceAccountPair<P>,
        freshness: EvidenceFreshnessKey<P>,
        digest: EvidenceDigest,
        canonical_receipt: Vec<u8>,
        observed_at: UnixTime,
    ) -> Self {
        Self {
            pair,
            freshness,
            digest,
            canonical_receipt,
            observed_at,
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

    fn byte_len(&self) -> usize {
        self.canonical_receipt.len()
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
}

impl Default for EvidenceLimits {
    fn default() -> Self {
        Self {
            max_records: nonzero(4_096),
            max_bytes: nonzero(16 * 1024 * 1024),
            max_records_per_pair: nonzero(64),
            max_bytes_per_pair: nonzero(1024 * 1024),
        }
    }
}

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
    admitted: u64,
    duplicates: u64,
    conflicts: u64,
    evicted_records: u64,
    evicted_bytes: u64,
    rejected_records: u64,
}

impl EvidenceCounters {
    /// Newly admitted receipts.
    pub const fn admitted(self) -> u64 {
        self.admitted
    }

    /// Exact duplicate receipts rejected.
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
}

/// Admission outcome for a candidate receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceAdmission {
    /// The candidate was admitted, possibly evicting older evidence.
    Admitted,
    /// The same canonical digest was already resident.
    Duplicate,
    /// The freshness key was already occupied by a different receipt.
    Conflict {
        /// Digest retained for the freshness key.
        retained: EvidenceDigest,
    },
}

/// One deterministically evicted evidence record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidenceEviction<P> {
    pair: EvidenceAccountPair<P>,
    digest: EvidenceDigest,
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
    admission: EvidenceAdmission,
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
    /// Aggregate counters without account labels.
    pub counters: EvidenceCounters,
}

/// Summary of invalid or over-bound snapshot entries skipped during restore.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EvidenceLoadReport {
    rejected_records: usize,
    evicted_records: usize,
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
}

/// One bounded page in canonical digest order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EvidencePage<P> {
    records: Vec<ProvisionalEvidenceRecord<P>>,
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
}

/// Pure bounded provisional-evidence state.
pub struct ProvisionalEvidenceStore<P> {
    limits: EvidenceLimits,
    records: BTreeMap<EvidenceDigest, ProvisionalEvidenceRecord<P>>,
    freshness: BTreeMap<EvidenceFreshnessKey<P>, EvidenceDigest>,
    resident_bytes: usize,
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
            resident_bytes: 0,
            counters: EvidenceCounters::default(),
        }
    }

    /// Restore valid entries independently so one malformed record cannot hide another.
    pub fn from_snapshot_with_validator(
        snapshot: EvidenceSnapshot<P>,
        limits: EvidenceLimits,
        validator: impl Fn(&ProvisionalEvidenceRecord<P>) -> bool,
    ) -> Result<(Self, EvidenceLoadReport), EvidenceError> {
        if snapshot.schema_version != EVIDENCE_SNAPSHOT_VERSION {
            return Err(EvidenceError::UnsupportedSnapshotVersion {
                found: snapshot.schema_version,
            });
        }
        let prior_counters = snapshot.counters;
        let mut records = snapshot.records;
        records.sort_by_key(|record| (record.observed_at(), record.digest()));
        let mut store = Self::new(limits);
        let mut report = EvidenceLoadReport::default();
        for record in records {
            if !validator(&record) {
                report.rejected_records = report.rejected_records.saturating_add(1);
                continue;
            }
            match store.admit(record) {
                Ok(admission) if admission.admission == EvidenceAdmission::Admitted => {
                    report.evicted_records = report
                        .evicted_records
                        .saturating_add(admission.evictions.len());
                }
                Ok(_) | Err(_) => {
                    report.rejected_records = report.rejected_records.saturating_add(1);
                }
            }
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
        };
        Ok((store, report))
    }

    /// Apply one admission transition.
    pub fn admit(
        &mut self,
        record: ProvisionalEvidenceRecord<P>,
    ) -> Result<EvidenceAdmissionReport<P>, EvidenceError> {
        self.validate_size(&record)?;
        if self.records.contains_key(&record.digest) {
            self.counters.duplicates = self.counters.duplicates.saturating_add(1);
            return Ok(EvidenceAdmissionReport {
                admission: EvidenceAdmission::Duplicate,
                evictions: Vec::new(),
            });
        }
        if let Some(retained) = self.freshness.get(&record.freshness).copied() {
            self.counters.conflicts = self.counters.conflicts.saturating_add(1);
            return Ok(EvidenceAdmissionReport {
                admission: EvidenceAdmission::Conflict { retained },
                evictions: Vec::new(),
            });
        }

        let pair = record.pair.clone();
        let digest = record.digest;
        self.resident_bytes = self.resident_bytes.saturating_add(record.byte_len());
        self.freshness.insert(record.freshness.clone(), digest);
        self.records.insert(digest, record);
        self.counters.admitted = self.counters.admitted.saturating_add(1);

        let mut evictions = Vec::new();
        while self.pair_over_bound(&pair) {
            let Some(victim) = self.oldest_digest(Some(&pair)) else {
                break;
            };
            if let Some(eviction) = self.evict(victim) {
                evictions.push(eviction);
            }
        }
        while self.global_over_bound() {
            let Some(victim) = self.oldest_digest(None) else {
                break;
            };
            if let Some(eviction) = self.evict(victim) {
                evictions.push(eviction);
            }
        }
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

    /// Produce the deterministic persistence form.
    pub fn snapshot(&self) -> EvidenceSnapshot<P> {
        EvidenceSnapshot {
            schema_version: EVIDENCE_SNAPSHOT_VERSION,
            records: self.records.values().cloned().collect(),
            counters: self.counters,
        }
    }

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

    fn pair_over_bound(&self, pair: &EvidenceAccountPair<P>) -> bool {
        let (records, bytes) = self.pair_usage(pair);
        records > self.limits.max_records_per_pair.get()
            || bytes > self.limits.max_bytes_per_pair.get()
    }

    fn global_over_bound(&self) -> bool {
        self.records.len() > self.limits.max_records.get()
            || self.resident_bytes > self.limits.max_bytes.get()
    }

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

    fn oldest_digest(&self, pair: Option<&EvidenceAccountPair<P>>) -> Option<EvidenceDigest> {
        self.records
            .iter()
            .filter(|(_, record)| pair.is_none_or(|pair| &record.pair == pair))
            .min_by_key(|(digest, record)| (record.observed_at, **digest))
            .map(|(digest, _)| *digest)
    }

    fn evict(&mut self, digest: EvidenceDigest) -> Option<EvidenceEviction<P>> {
        let record = self.records.remove(&digest)?;
        let bytes = record.byte_len();
        self.freshness.remove(&record.freshness);
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
