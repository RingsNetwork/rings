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
//! - Removal: a tombstone leaves the carrier's bound unchanged. Retention is refreshed by what
//!   is held (adds and overwrites), never by a removal's own bound; a data carrier drained to
//!   tombstones stays live only until its removes are stable (see Liveness).
//! - Liveness: `is_live_at(now) ⟺ expires_at_ms = Some(t) ∧ (now < t ∨ now < stable(x))`,
//!   where `stable(x) = max τ(k) + H + σ` over the removes a data carrier holds (see Element
//!   horizon) and is absent for a relay inbox. A carrier that expired with an unstable remove
//!   would take the remove with it before its horizon, and a replica whose carrier other writes
//!   keep alive would then serve the removed payload back. An unstamped value is not live, so a
//!   stored value that predates retention is retired on its next read.
//! - Admission: a value is admissible at `now` in overlay `n` iff it is live, its bound is at
//!   most `now + kind.max_lifetime_ms() + TS_OFFSET_TOLERANCE_MS`, every version it carries has
//!   a logical time at most `now + TS_OFFSET_TOLERANCE_MS`, every payload is at most
//!   `ENTRY_PAYLOAD_MAX_BYTES`, and its kind's witness holds (a relay inbox admits only held
//!   messages addressed to its recipient, attested by their holder and verified as of the hold
//!   instant inside `n`; see the `inbox` module). Admission is a predicate on the
//!   peer-supplied delta, never on the receiver's join result, so locally derived versions (an
//!   overwrite floor bumped by one step) are never mistaken for a peer clock running ahead.
//!
//! # Element horizon
//!
//! The carrier bound joins by `max`, so a data carrier that other writers keep refreshing never
//! expires, and neither would its elements or its tombstones. A tombstone may be dropped only
//! once no carrier anywhere can still hold an add it covers as live; otherwise a stale replica
//! resurrects the add. Acknowledgements cannot establish that (replica sets move with churn,
//! and fetch caches are carriers that never acknowledge), and a reset floor stamped by one
//! owner establishes it only by erasing every concurrent add that owner never received (#867).
//! The stability used here is a clock horizon instead (#872):
//!
//! ```text
//!   τ(d) = d.version.logical_time_ms          the dot's issue time
//!   H    = kind.element_horizon_ms()          Data: max_lifetime_ms, no single write asks longer
//!   σ    = TS_OFFSET_TOLERANCE_MS             assumed bound on pairwise clock skew
//!
//!   retire_t(x) = x ∖ { add (v, d)        | t ≥ τ(d) + H     }
//!                   ∖ { remove (e, r)     | t ≥ τ(r) + H + σ }
//! ```
//!
//! `retire_t` is applied at the local clock `t` on every read of a stored or cached carrier and
//! to every value written to storage or to the fetch cache. So every data element expires
//! individually at `τ(d) + H` unless it is written again (a re-append issues a fresh dot), even
//! while other writes keep its carrier alive: a registry refreshes its descriptors every
//! heartbeat, and a plain `append` publisher must refresh its values within `H`.
//!
//! Laws, for `t` any node's clock and `x, y` carriers of one data topic:
//! - Homomorphism: `retire_t(x ⊔ y) = retire_t(x) ⊔ retire_t(y)`. Both filters are thresholds
//!   on `τ`, the leading component of the dot order, so each drops a down-set of dots; `max`
//!   per payload commutes with dropping a down-set; and a dropped remove only ever covered adds
//!   that are dropped too (`d ≤ r ⇒ τ(d) ≤ τ(r)` and `H ≤ H + σ`), so no join pairs a
//!   surviving add with a dropped remove that covered it. Retirement therefore commutes with
//!   every replication path, and replicas stay join-compatible.
//! - Composition: `retire_t ∘ retire_s = retire_max(s, t)`; in particular `retire_t` is
//!   idempotent.
//! - No loss: no live add is lost. An add leaves a carrier only by a remove covering it, a user
//!   `Overwrite` register above it, the `max_data_len` cap, or its own horizon:
//!   `∀ a. t < τ(a) + H ∧ ¬covered(a) ∧ a ∈ ⋃ᵢ xᵢ ⇒ a ∈ retire_t(⨆ᵢ xᵢ)`, cap aside.
//! - No resurrection: a remove `(e, r)` is dropped, by this projection or with its carrier (which
//!   stays live while it holds the remove, see Liveness), only at a clock `t_p ≥ τ(r) + H + σ`.
//!   Every clock then reads `t_q ≥ t_p − σ ≥ τ(r) + H ≥ τ(d) + H` for every add `d ≤ r` it
//!   covered, including the dropping node's own clock should it step back by up to `σ`, so every
//!   carrier has already retired those adds and none can serve them back.
//! - Bound: a data carrier holds at most `max_data_len` elements, and at most one remove per
//!   payload removed within the last `H + σ`. This is a rate bound, not a count cap: a cap
//!   would drop a remove whose adds are still inside their horizon somewhere.
//!
//! A relay inbox has no horizon: it has no reset floor, its tombstones are capped, and a held
//! message lives as long as its carrier (see the `inbox` module).
//!
//! Size: the payload predicate is element-intrinsic, so filtering by it commutes with union
//! and the carrier stays a lattice; together with the count cap `kind.max_data_len()` it bounds
//! a carrier at `max_data_len × ENTRY_PAYLOAD_MAX_BYTES` encoded bytes. A byte budget
//! over the whole carrier is deliberately not used: "the newest payloads that fit" depends on
//! the sizes of payloads a replica may already have dropped, so it is not a lattice morphism
//! and replicas would diverge.

use super::DataTopicBuffer;
use super::Entry;
use super::EntryDot;
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

    /// The element horizon `H`: how long after its dot's issue time an add of this kind may be
    /// held anywhere, or `None` for a kind whose removals are bounded otherwise.
    ///
    /// A data element lives at most the longest retention one write may request, so writing a
    /// value again (which issues a fresh dot) is what keeps it; a relay inbox has none (see the
    /// module documentation).
    pub const fn element_horizon_ms(self) -> Option<u64> {
        match self {
            EntryKind::Data => Some(self.max_lifetime_ms()),
            EntryKind::RelayMessage => None,
        }
    }
}

/// The clock thresholds of `retire_t` for one horizon `H`.
#[derive(Clone, Copy, Debug)]
struct RetirementHorizon {
    /// `H` in milliseconds.
    horizon_ms: u128,
}

impl RetirementHorizon {
    /// Whether the add `dot` is past the horizon at `now_ms`: `t ≥ τ(d) + H`.
    fn retires_add(self, dot: &EntryDot, now_ms: u128) -> bool {
        now_ms >= dot.version.logical_time_ms.saturating_add(self.horizon_ms)
    }

    /// The instant the remove at `dot` becomes stable, i.e. every add it covers is retired on
    /// every clock within the skew tolerance: `τ(r) + H + σ`.
    fn remove_stable_at(self, dot: &EntryDot) -> u128 {
        dot.version
            .logical_time_ms
            .saturating_add(self.horizon_ms)
            .saturating_add(TS_OFFSET_TOLERANCE_MS)
    }

    /// Whether the remove at `dot` is stable at `now_ms`: `t ≥ τ(r) + H + σ`.
    fn retires_remove(self, dot: &EntryDot, now_ms: u128) -> bool {
        now_ms >= self.remove_stable_at(dot)
    }

    /// `retire_t` on a normalized buffer at the clock `now_ms`.
    fn retire(self, buffer: DataTopicBuffer, now_ms: u128) -> DataTopicBuffer {
        let DataTopicBuffer {
            register,
            mut values,
            mut removes,
        } = buffer;
        values.retain(|_, dot| !self.retires_add(dot, now_ms));
        removes.retain(|_, dot| !self.retires_remove(dot, now_ms));
        DataTopicBuffer::new(register, values, removes)
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

    /// The element-horizon projection `retire_t` at the local clock `now_ms`, normalized for
    /// storage (see the module documentation).
    ///
    /// Post: the result is [`Self::try_into_storage_entry`] of `self` without every add past
    /// its horizon and every stable remove; for a kind without a horizon it is exactly
    /// [`Self::try_into_storage_entry`]. The register and the retention bound are unchanged.
    pub fn retired_at(self, now_ms: u128) -> Result<Self> {
        let buffer = self.topic_buffer()?;
        let buffer = match self.kind.element_horizon_ms() {
            Some(horizon_ms) => RetirementHorizon {
                horizon_ms: u128::from(horizon_ms),
            }
            .retire(buffer, now_ms),
            None => buffer,
        };
        Ok(self.materialize_topic_buffer(buffer, self.expires_at_ms))
    }

    /// Whether this entry may still be served or replicated at `now_ms`: its retention bound
    /// has not elapsed, or it still holds a remove that is not yet stable (see the module
    /// documentation).
    ///
    /// Post: `false` for an unstamped entry, so a legacy stored value without a bound is
    /// retired on its next read.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms.is_some_and(|expires_at_ms| {
            now_ms < expires_at_ms
                || self
                    .removes_stable_at()
                    .is_some_and(|stable_at_ms| now_ms < stable_at_ms)
        })
    }

    /// The instant every remove this entry holds is stable, `max τ(k) + H + σ`, or `None` for a
    /// kind without an element horizon or an entry that holds no remove.
    fn removes_stable_at(&self) -> Option<u128> {
        let horizon = RetirementHorizon {
            horizon_ms: u128::from(self.kind.element_horizon_ms()?),
        };
        self.crdt
            .tombstones
            .iter()
            .map(|tombstone| horizon.remove_stable_at(&tombstone.dot))
            .max()
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
        self.validate_bounds_at(now_ms)?;
        match self.kind {
            EntryKind::Data => Ok(()),
            EntryKind::RelayMessage => self.validate_inbox_witness(now_ms, network_id),
        }
    }

    /// The kind-independent part of the admission law: liveness, retention bound, version
    /// clocks, and payload sizes.
    pub(crate) fn validate_bounds_at(&self, now_ms: u128) -> Result<()> {
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
        Ok(())
    }
}

impl EntryOperation {
    /// Admission law for the delta this operation carries.
    pub fn validate_admissible_at(&self, now_ms: u128, network_id: u32) -> Result<()> {
        self.entry().validate_admissible_at(now_ms, network_id)
    }
}
