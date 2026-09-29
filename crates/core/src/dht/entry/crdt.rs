//! CRDT carriers for DHT entries.
//!
//! State variables:
//! - `register` is an optional LWW reset floor for overwrite.
//! - `values` is an LWW element set keyed by encoded payload: each payload keeps its greatest
//!   add dot.
//! - `removes` is the remove side, keyed by payload digest: each digest keeps its greatest
//!   remove dot, and a remove `(e, r)` covers every add `(v, d)` with `digest(v) = e ∧ d ≤ r`.
//!
//! ```text
//!   covered(v, d)  ≜  ∃ r. removes[digest(v)] = r ∧ d ≤ r
//!   visible(v, d)  ≜  values[v] = d ∧ (register = ⊥ ∨ register ≤ d.version) ∧ ¬covered(v, d)
//! ```
//!
//! A remove covers a payload's dots up to its own, not one exact dot, because `values` keeps
//! only the greatest dot per payload: a dot superseded by a later dot of the same payload is
//! forgotten, so a remove naming only the surviving dot would let a stale replica that still
//! holds the superseded one resurrect the payload (#874). Under the covering remove the
//! forgotten dot needs no storage at all, and the carrier is an LWW element set: a remove at
//! `r` wins over every add of the payload at or below `r`, an add above `r` wins over it.
//! "At or below" is the dot order, which leads with the hybrid logical time: a re-add issued
//! after the remove in real time by a writer whose clock runs behind (by at most `σ`) can lie
//! below `r` and stays removed. A value-witness remove carries no dot, so each owner covers the
//! dot it holds when the remove arrives; delivered after the same client's re-add, it covers
//! that re-add too. A remove that must not outrun its issuer's view names the issuer's dots
//! (the dot-witness path of `Entry::tombstone`).
//!
//! Semilattice laws:
//! - `DataTopicBuffer` join is the product of three join-semilattices, `register` under `max`
//!   and `values`, `removes` under pointwise `max`, followed by the normalization
//!   `DataTopicBuffer::new`. The normalization is a closure operator whose dropped elements stay
//!   dropped under every further join (a floor and a covering remove only grow), so the join is
//!   idempotent, commutative, and associative over normalized carriers. A data topic and a relay
//!   inbox are the same carrier; their per-kind authority, tombstone cap, and element horizon
//!   are enforced by the entry, not by a second set type.
//!
//! Constructor postconditions:
//! - `DataTopicBuffer::new` preserves only values and removes whose dot is at or after the reset
//!   floor when a reset floor exists, and only values no remove covers.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use crate::algebra::JoinSemilattice;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;

/// Hybrid logical version for LWW entry registers and element dots.
///
/// `logical_time_ms` starts from the wall-clock millisecond observed at the
/// storage-operation boundary, then advances beyond any local floor that would
/// otherwise dominate it. `actor` and `operation` make concurrent writes from
/// the same millisecond totally ordered without claiming wall-clock recency.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryVersion {
    /// Hybrid logical time in milliseconds.
    #[serde(alias = "epoch_ms")]
    pub logical_time_ms: u128,
    /// Storage node that first stamped the operation.
    pub actor: Did,
    /// Deterministic digest of the stamped operation payload.
    #[serde(default)]
    pub operation: Did,
}

impl EntryVersion {
    /// Construct a version from an explicit hybrid logical time and actor.
    pub fn new(logical_time_ms: u128, actor: Did, operation: Did) -> Self {
        Self {
            logical_time_ms,
            actor,
            operation,
        }
    }

    pub(super) fn after(self, floor: Option<Self>) -> Self {
        let Some(floor) = floor else {
            return self;
        };
        if self > floor {
            return self;
        }
        Self {
            logical_time_ms: floor.logical_time_ms.saturating_add(1),
            actor: self.actor,
            operation: self.operation,
        }
    }
}

/// Unique add witness for one visible entry payload element.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryDot {
    /// LWW version that issued this element.
    pub version: EntryVersion,
    /// Element position inside the issuing operation.
    pub index: u32,
}

impl EntryDot {
    pub(super) fn for_index(version: EntryVersion, index: usize) -> Result<Self> {
        let index = u32::try_from(index).map_err(|_| Error::EntryDotIndexOutOfBounds { index })?;
        Ok(Self { version, index })
    }
}

/// Content address of one element payload: the Keccak-256 digest of its encoded form.
///
/// A remove names the payload it removes by this digest rather than by its bytes, so a
/// tombstone costs a constant number of bytes whatever the payload size.
///
/// Law: `ElementDigest::of(u) = ElementDigest::of(v) ⟺ u = v`, up to Keccak-256 collision
/// resistance.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct ElementDigest(pub [u8; 32]);

#[cfg(test)]
thread_local! {
    /// Test instrumentation: the element digests computed on this thread, so a test can bound
    /// the hashing an operation performs.
    pub(crate) static DIGESTS_COMPUTED: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

impl ElementDigest {
    /// The digest of `value`.
    pub fn of(value: &Encoded) -> Self {
        #[cfg(test)]
        DIGESTS_COMPUTED.with(|computed| computed.set(computed.get() + 1));
        Self(keccak256(value.value().as_bytes()))
    }
}

/// Remove witness for one payload: it covers every add of the payload whose digest is `element`
/// at or below `dot` (see the module documentation).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryTombstone {
    /// Digest of the removed payload.
    pub element: ElementDigest,
    /// Greatest add dot of the payload the remove covers.
    pub dot: EntryDot,
}

impl EntryTombstone {
    /// Remove witness covering the adds of `value` at or below `dot`.
    pub fn of(value: &Encoded, dot: EntryDot) -> Self {
        Self {
            element: ElementDigest::of(value),
            dot,
        }
    }
}

/// CRDT metadata carried beside the legacy entry payload.
///
/// `register` is the LWW reset floor used by overwrite. `dots` are per-element
/// add witnesses used by data/topic and relay element sets. `tombstones` is the
/// remove side: one covering remove per removed payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryCrdt {
    /// Optional LWW reset floor for the entry payload.
    pub register: Option<EntryVersion>,
    /// Per-element add dots. When absent, legacy entries synthesize dots from
    /// their payload order and value digest.
    pub dots: Vec<EntryDot>,
    /// Covering removes, at most one per payload digest.
    pub tombstones: Vec<EntryTombstone>,
}

impl EntryCrdt {
    pub(super) fn has_write_witness(&self) -> bool {
        self.register.is_some() || !self.dots.is_empty()
    }

    /// Return the bottom floor used only to lift legacy payloads without dots.
    pub(super) fn legacy_floor(&self) -> EntryVersion {
        self.register.unwrap_or_default()
    }
}

/// Bounded LWW element set used by data topic buffers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataTopicBuffer {
    pub(super) register: Option<EntryVersion>,
    pub(super) values: BTreeMap<Encoded, EntryDot>,
    pub(super) removes: BTreeMap<ElementDigest, EntryDot>,
}

/// Insert `dot` at `key`, keeping the greater dot when `key` is already present: the join of
/// one singleton into a pointwise-`max` map.
pub(super) fn insert_max<K: Ord>(map: &mut BTreeMap<K, EntryDot>, key: K, dot: EntryDot) {
    map.entry(key)
        .and_modify(|current| *current = (*current).max(dot))
        .or_insert(dot);
}

impl DataTopicBuffer {
    /// Normalize a carrier: drop what the reset floor or a covering remove shadows.
    ///
    /// Post: every kept value and remove has a dot at or after `register`, and no kept value is
    /// covered by a kept remove.
    pub(super) fn new(
        register: Option<EntryVersion>,
        mut values: BTreeMap<Encoded, EntryDot>,
        mut removes: BTreeMap<ElementDigest, EntryDot>,
    ) -> Self {
        if let Some(floor) = register {
            values.retain(|_, dot| dot.version >= floor);
            removes.retain(|_, dot| dot.version >= floor);
        }
        values.retain(|value, dot| !Self::covered_by(&removes, value, *dot));
        Self {
            register,
            values,
            removes,
        }
    }

    /// Whether a remove in `removes` covers the add `(value, dot)`.
    pub(super) fn covered_by(
        removes: &BTreeMap<ElementDigest, EntryDot>,
        value: &Encoded,
        dot: EntryDot,
    ) -> bool {
        !removes.is_empty()
            && removes
                .get(&ElementDigest::of(value))
                .is_some_and(|remove| dot <= *remove)
    }

    /// Record a covering remove for `value` at its held dot, if the carrier holds `value`.
    pub(super) fn remove(&mut self, value: &Encoded) {
        if let Some(dot) = self.values.remove(value) {
            insert_max(&mut self.removes, ElementDigest::of(value), dot);
        }
    }
}

impl JoinSemilattice for DataTopicBuffer {
    fn join(mut self, other: Self) -> Self {
        self.register = self.register.max(other.register);
        for (element, dot) in other.removes {
            insert_max(&mut self.removes, element, dot);
        }
        for (value, dot) in other.values {
            insert_max(&mut self.values, value, dot);
        }
        Self::new(self.register, self.values, self.removes)
    }
}
