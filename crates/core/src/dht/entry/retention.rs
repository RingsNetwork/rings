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
//!   is held (adds and overwrites), never by a removal: once the bound elapses, a data carrier
//!   serves no element, and only its remove side stays until it is stable (see Liveness).
//! - Liveness: `is_live_at(now) ⟺ expires_at_ms = Some(t) ∧ (now < t ∨ now < stable(x))`,
//!   where `stable(x) = max({τ(k)} ∪ {τ(register)}) + H + σ` over the removes and the register
//!   a data carrier holds (see Element horizon), and is absent for a relay inbox. A carrier
//!   that expired with an unstable remove would take the remove with it before its horizon, and
//!   a replica whose carrier other writes keep alive would then serve the removed payload back;
//!   the register counts because an overwrite drops every remove below it. Past `t`, the
//!   projection empties the element side (`retired_at`), so a live-by-`stable` carrier serves
//!   only its removes and register. An unstamped value is not live, so a stored value that
//!   predates retention is retired on its next read.
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
//!                   ∖ { register F        | t ≥ τ(F) + H + σ }
//!
//!   retired_at(x, t) = retire_t(x)                          if t < x.expires_at_ms
//!                      retire_t(x) with no elements         otherwise
//! ```
//!
//! `retired_at` is applied at the local clock `t` on every read of a stored or cached carrier
//! and to every value written to storage or to the fetch cache. So every data element lives
//! until the earlier of `τ(d) + H` and its carrier's retention bound, the latest bound any write
//! it was joined with requested (a plain append requests `default_lifetime_ms`, 10 minutes).
//! Writing it again issues a fresh dot and a fresh bound: a sole writer must rewrite a value
//! within the default lifetime, and every writer within `H`, even while other writes keep the
//! carrier alive. A registry refreshes its descriptors every heartbeat.
//!
//! Laws, for `t` any node's clock and `x, y` carriers of one data topic:
//! - Homomorphism: `retire_t(x ⊔ y) = retire_t(x) ⊔ retire_t(y)`. Every filter is a threshold
//!   on `τ`, the leading component of the dot order, so each drops a down-set of dots; `max`
//!   per payload (and on the register) commutes with dropping a down-set; and a dropped remove
//!   or register only ever covered or floored adds that are dropped too (`d ≤ r ⇒ τ(d) ≤ τ(r)`
//!   and `H ≤ H + σ`), so no join pairs a surviving add with a dropped remove or floor that
//!   shadowed it. Retirement therefore commutes with every replication path, and replicas stay
//!   join-compatible. The bound-elapsed step of `retired_at` is carrier-level, like deleting an
//!   expired carrier, and is not a homomorphism; it only narrows that deletion.
//! - Composition: `retire_t ∘ retire_s = retire_max(s, t)`; in particular `retire_t` is
//!   idempotent.
//! - No loss: no live add is lost. An add leaves a carrier only by a remove covering it, a user
//!   `Overwrite` register above it, the `max_data_len` cap, its own horizon, its carrier's
//!   retention bound, or storage byte-budget eviction of the whole carrier:
//!   `∀ a. t < τ(a) + H ∧ ¬covered(a) ∧ ¬(a.version < register(⨆ᵢ xᵢ)) ∧ a ∈ ⋃ᵢ xᵢ
//!   ⇒ a ∈ retire_t(⨆ᵢ xᵢ)`, cap aside.
//! - No resurrection: a remove `(e, r)` or a register leaves the replicas that hold it only at a
//!   clock `t_p ≥ τ(r) + H + σ`, whether by this projection, by an overwrite above it (whose
//!   register then holds the carrier at least as long), or with its carrier (which stays live
//!   while it holds either, see Liveness); an ack-gated hand-off deletes the sender's copy only
//!   once the receiver has joined it, so it moves rather than drops. Every clock then reads
//!   `t_q ≥ t_p − σ ≥ τ(r) + H ≥ τ(d) + H` for every add `d ≤ r` it shadowed, including the
//!   dropping node's own clock should it step back by up to `σ`, so every carrier has already
//!   retired those adds and none can serve them back. This holds barring storage byte-budget
//!   eviction, which drops a carrier whatever it holds (see SECURITY.md).
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

use super::Entry;
use super::EntryCrdt;
use super::EntryDot;
use super::EntryKind;
use super::EntryOperation;
use super::EntryVersion;
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

/// The retention law of a kind with an element horizon: when an add retires, when a remove (and
/// the register) retires, and what holds an expired carrier live.
///
/// The production law is [`ElementRetention::of`]. The horizons are crate-visible so a model can
/// check a deliberately broken law and witness that it fails; the two holders, fixed in
/// production, exist only in test builds for the same purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ElementRetention {
    /// `H`: an add retires at `τ(d) + H`.
    pub(crate) add_horizon_ms: u128,
    /// `H + σ`: a remove, and the register, retire at `τ + H + σ`.
    pub(crate) remove_horizon_ms: u128,
    /// Whether an unstable remove keeps a carrier live past its retention bound.
    #[cfg(test)]
    pub(crate) removes_hold_carrier: bool,
    /// Whether an unstable register keeps a carrier live past its retention bound.
    #[cfg(test)]
    pub(crate) register_holds_carrier: bool,
}

impl ElementRetention {
    /// The production law of `kind`, or `None` for a kind without an element horizon.
    pub(crate) fn of(kind: EntryKind) -> Option<Self> {
        let horizon_ms = u128::from(kind.element_horizon_ms()?);
        Some(Self {
            add_horizon_ms: horizon_ms,
            remove_horizon_ms: horizon_ms.saturating_add(TS_OFFSET_TOLERANCE_MS),
            #[cfg(test)]
            removes_hold_carrier: true,
            #[cfg(test)]
            register_holds_carrier: true,
        })
    }

    /// Whether an unstable remove, and whether an unstable register, keeps a carrier live past
    /// its retention bound: both, under the production law.
    #[cfg(not(test))]
    const fn holders(self) -> (bool, bool) {
        (true, true)
    }

    /// Whether an unstable remove, and whether an unstable register, keeps a carrier live past
    /// its retention bound, as the law under test sets them.
    #[cfg(test)]
    const fn holders(self) -> (bool, bool) {
        (self.removes_hold_carrier, self.register_holds_carrier)
    }

    /// Whether the add `dot` is past the horizon at `now_ms`: `t ≥ τ(d) + H`.
    fn retires_add(self, dot: &EntryDot, now_ms: u128) -> bool {
        now_ms
            >= dot
                .version
                .logical_time_ms
                .saturating_add(self.add_horizon_ms)
    }

    /// The instant a remove or register at `version` becomes stable, i.e. every add it covers
    /// or floors is retired on every clock within the skew tolerance: `τ + H + σ`.
    fn stable_at(self, version: &EntryVersion) -> u128 {
        version
            .logical_time_ms
            .saturating_add(self.remove_horizon_ms)
    }

    /// Whether a remove or register at `version` is stable at `now_ms`: `t ≥ τ + H + σ`.
    fn retires_remove(self, version: &EntryVersion, now_ms: u128) -> bool {
        now_ms >= self.stable_at(version)
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

    /// The element-horizon projection at the local clock `now_ms` (see the module
    /// documentation): `retire_t`, then, once the retention bound has elapsed, the carrier's
    /// elements dropped so that only its remove side outlives the bound.
    ///
    /// Every stored value, join result, and operation result is normalized for storage
    /// ([`Self::try_into_storage_entry`]), and the projection is then a pure filter. An entry
    /// whose elements and dots are misaligned (a value stored by an earlier build) is
    /// normalized first, and if even that fails its elements, which carry no provable dot, are
    /// dropped.
    /// Post: normalized; identity for a kind without a horizon, and identity, without copying or
    /// hashing, for a normalized entry in which nothing has crossed a threshold. The retention
    /// bound is unchanged.
    pub fn retired_at(self, now_ms: u128) -> Self {
        let retention = ElementRetention::of(self.kind);
        self.retired_under(retention, now_ms)
    }

    /// [`Self::retired_at`] under the retention law `retention`.
    pub(super) fn retired_under(self, retention: Option<ElementRetention>, now_ms: u128) -> Self {
        let Some(retention) = retention else {
            return self;
        };
        let bound_elapsed = !self.bound_live_at(now_ms);
        let entry = self.aligned().horizon_retired_under(retention, now_ms);
        match bound_elapsed && !entry.data.is_empty() {
            true => entry.without_elements(),
            false => entry,
        }
    }

    /// This entry with one dot per element: itself when aligned, else its normalization, else
    /// (when normalization fails) itself without elements.
    fn aligned(self) -> Self {
        if self.data.len() == self.crdt.dots.len() {
            return self;
        }
        match self.clone().try_into_storage_entry() {
            Ok(normalized) => normalized,
            Err(_) => self.without_elements(),
        }
    }

    /// This entry with its element side emptied and its remove side, register and bound kept.
    fn without_elements(self) -> Self {
        Self {
            data: Vec::new(),
            crdt: EntryCrdt {
                dots: Vec::new(),
                ..self.crdt
            },
            ..self
        }
    }

    /// `retire_t` alone under `retention`: every add past `τ + H` and every remove and register
    /// past `τ + H + σ` is dropped. This is the join homomorphism of the module documentation;
    /// the bound-elapsed projection of [`Self::retired_at`] is not.
    ///
    /// Pre: `self` is normalized. Retiring a remove or the register never uncovers an element,
    /// since every element either covered is retired first, so the filters need no
    /// renormalization and compute no digest.
    pub(super) fn horizon_retired_under(self, retention: ElementRetention, now_ms: u128) -> Self {
        let retires_add = |dot: &EntryDot| retention.retires_add(dot, now_ms);
        let retires_remove = |version: &EntryVersion| retention.retires_remove(version, now_ms);
        let crossed = self.crdt.dots.iter().any(retires_add)
            || self
                .crdt
                .tombstones
                .iter()
                .any(|tombstone| retires_remove(&tombstone.dot.version))
            || self.crdt.register.as_ref().is_some_and(retires_remove);
        if !crossed {
            return self;
        }
        let (data, dots) = self
            .data
            .into_iter()
            .zip(self.crdt.dots)
            .filter(|(_, dot)| !retires_add(dot))
            .unzip();
        Self {
            data,
            crdt: EntryCrdt {
                register: self
                    .crdt
                    .register
                    .filter(|version| !retires_remove(version)),
                dots,
                tombstones: self
                    .crdt
                    .tombstones
                    .into_iter()
                    .filter(|tombstone| !retires_remove(&tombstone.dot.version))
                    .collect(),
            },
            ..self
        }
    }

    /// Whether this carrier answers a lookup at `now_ms` as found: its retention bound has not
    /// elapsed. A carrier held live past its bound by an unstable remove or register serves no
    /// element, and answering it as found would shadow a replica that still holds data; it
    /// answers as absent, and its remove side still spreads by join on hand-off and repair.
    pub fn answers_lookups_at(&self, now_ms: u128) -> bool {
        self.bound_live_at(now_ms)
    }

    /// Whether the retention bound itself has not elapsed at `now_ms`.
    fn bound_live_at(&self, now_ms: u128) -> bool {
        self.expires_at_ms
            .is_some_and(|expires_at_ms| now_ms < expires_at_ms)
    }

    /// Whether this entry may still be served or replicated at `now_ms`: its retention bound
    /// has not elapsed, or it still holds a remove or register that is not yet stable (see the
    /// module documentation).
    ///
    /// Post: `false` for an unstamped entry, so a legacy stored value without a bound is
    /// retired on its next read.
    pub fn is_live_at(&self, now_ms: u128) -> bool {
        self.is_live_under(ElementRetention::of(self.kind), now_ms)
    }

    /// [`Self::is_live_at`] under the retention law `retention`.
    pub(super) fn is_live_under(&self, retention: Option<ElementRetention>, now_ms: u128) -> bool {
        self.expires_at_ms.is_some()
            && (self.bound_live_at(now_ms)
                || retention
                    .and_then(|retention| self.removes_stable_under(retention))
                    .is_some_and(|stable_at_ms| now_ms < stable_at_ms))
    }

    /// The instant every remove and the register this entry holds are stable,
    /// `max({τ(k)} ∪ {τ(register)}) + H + σ`, or `None` when it holds neither (or when
    /// `retention` lets nothing hold the carrier).
    ///
    /// The register counts because an overwrite drops every remove below it: were it not to
    /// hold the carrier, an overwrite would collapse an unstable remove into a carrier that
    /// expires at its own bound, and a stale replica could serve the removed payload back.
    fn removes_stable_under(&self, retention: ElementRetention) -> Option<u128> {
        let (removes_hold, register_holds) = retention.holders();
        let removes = self
            .crdt
            .tombstones
            .iter()
            .map(|tombstone| tombstone.dot.version)
            .filter(|_| removes_hold);
        let register = self.crdt.register.filter(|_| register_holds);
        removes
            .chain(register)
            .map(|version| retention.stable_at(&version))
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
