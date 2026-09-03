//! Retention and admission of DHT entries.
//!
//! State: every entry carries `expires_at_ms : Option<u128>`, the instant after which it must
//! no longer be served or replicated. The origin stamps it at the operation boundary; every
//! receiver bounds it at admission. Both bounds are a property of the entry kind: a data topic
//! is refreshed by its publishers, while a relay inbox must outlive the absence of the peer it
//! is kept for (see the `inbox` module).
//!
//! Laws:
//! - Stamping: `stamped(now)` maps an absent bound to `now + kind.default_lifetime_ms()` and
//!   preserves a present one, so a forwarded operation keeps the origin's bound.
//! - Join: the bound of a join is the `max` of the bounds, with `None < Some(_)`. `max` is
//!   idempotent, commutative, and associative, so the product of the payload lattice and the
//!   bound lattice is again a join-semilattice.
//! - Liveness: `is_live_at(now) ⟺ expires_at_ms = Some(t) ∧ now < t`. An unstamped value is
//!   not live, so a stored value that predates retention is retired on its next read.
//! - Admission: a value is admissible at `now` in overlay `n` iff it is live, its bound is at
//!   most `now + kind.max_lifetime_ms() + TS_OFFSET_TOLERANCE_MS`, every version it carries has
//!   a logical time at most `now + TS_OFFSET_TOLERANCE_MS`, every payload is at most
//!   `ENTRY_PAYLOAD_MAX_BYTES`, and its kind's witness holds (a relay inbox admits only messages
//!   addressed to its owner and signed inside `n`). Admission is a predicate on the
//!   peer-supplied delta, never on the receiver's join result, so locally derived versions (a
//!   compaction floor bumped by one step) are never mistaken for a peer clock running ahead.
//!
//! Size: the payload predicate is element-intrinsic, so filtering by it commutes with union
//! and the carrier stays a lattice; together with the count cap `ENTRY_DATA_MAX_LEN` it bounds
//! a carrier at `ENTRY_DATA_MAX_LEN × ENTRY_PAYLOAD_MAX_BYTES` encoded bytes. A byte budget
//! over the whole carrier is deliberately not used: "the newest payloads that fit" depends on
//! the sizes of payloads a replica may already have dropped, so it is not a lattice morphism
//! and replicas would diverge.

use super::Entry;
use super::EntryKind;
use super::EntryOperation;
use crate::consts::DEFAULT_RELAY_INBOX_TTL_MS;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::ENTRY_PAYLOAD_MAX_BYTES;
use crate::consts::MAX_RELAY_INBOX_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;

impl EntryKind {
    /// Retention stamped at the operation boundary when the origin left it absent.
    pub const fn default_lifetime_ms(self) -> u64 {
        match self {
            EntryKind::Data => DEFAULT_TTL_MS,
            EntryKind::RelayMessage => DEFAULT_RELAY_INBOX_TTL_MS,
        }
    }

    /// The longest retention a receiver admits for this kind.
    pub const fn max_lifetime_ms(self) -> u64 {
        match self {
            EntryKind::Data => MAX_TTL_MS,
            EntryKind::RelayMessage => MAX_RELAY_INBOX_TTL_MS,
        }
    }
}

/// Whether one encoded payload is within the per-payload size bound.
fn payload_within_bound(value: &Encoded) -> bool {
    value.value().len() <= ENTRY_PAYLOAD_MAX_BYTES
}

impl Entry {
    /// Stamp the retention bound when the origin left it absent.
    pub(super) fn ensure_lifetime_from(mut self, now_ms: u128) -> Self {
        if self.expires_at_ms.is_none() {
            self.expires_at_ms =
                Some(now_ms.saturating_add(u128::from(self.kind.default_lifetime_ms())));
        }
        self
    }

    /// The retention bound of a join: the later of the two bounds.
    pub(super) fn joined_lifetime(&self, other: &Self) -> Option<u128> {
        self.expires_at_ms.max(other.expires_at_ms)
    }

    /// Whether this entry may still be served or replicated at `now_ms`.
    ///
    /// Post: `false` for an unstamped entry, so a legacy stored value without a bound is
    /// retired on its next read.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| now_ms < expires_at_ms)
    }

    /// Admission law for a delta supplied by a peer (see the module documentation).
    ///
    /// Pre: `now_ms` is the receiver's clock, `network_id` its overlay, and `self` is the
    /// peer-supplied value, not a join result.
    /// Post: `Ok` implies the entry is live, its bound and every version it carries are within
    /// the receiver's tolerance of `now_ms`, every payload is within the size bound, and the
    /// kind's witness holds. The version bound keeps a peer-supplied hybrid clock from pinning
    /// a key: an accepted floor can exceed the receiver's clock only by the message skew
    /// tolerance, so honest writers issued after that tolerance elapses dominate it again.
    pub fn validate_admissible_at(&self, now_ms: u128, network_id: u32) -> Result<()> {
        if !self.is_live_at(now_ms) {
            return Err(Error::EntryNotLive);
        }
        let lifetime_bound = now_ms
            .saturating_add(u128::from(self.kind.max_lifetime_ms()))
            .saturating_add(TS_OFFSET_TOLERANCE_MS);
        if self.expires_at_ms > Some(lifetime_bound) {
            return Err(Error::EntryLifetimeExceedsMax);
        }
        let clock_bound = now_ms.saturating_add(TS_OFFSET_TOLERANCE_MS);
        if self
            .versions()
            .any(|version| version.logical_time_ms > clock_bound)
        {
            return Err(Error::EntryVersionAheadOfClock);
        }
        if !self.data.iter().all(payload_within_bound) {
            return Err(Error::EntryPayloadExceedsMax);
        }
        match self.kind {
            EntryKind::Data => Ok(()),
            EntryKind::RelayMessage => self.validate_inbox_witness(network_id),
        }
    }
}

impl EntryOperation {
    /// Admission law for the delta this operation carries.
    pub fn validate_admissible_at(&self, now_ms: u128, network_id: u32) -> Result<()> {
        self.entry().validate_admissible_at(now_ms, network_id)
    }
}
