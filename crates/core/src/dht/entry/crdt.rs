//! CRDT carriers for DHT entries.
//!
//! State variables:
//! - `register` is an optional LWW reset floor for overwrite.
//! - `values` is an LWW element set keyed by element bytes: each payload keeps its greatest
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

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::algebra::JoinSemilattice;
use crate::dht::Did;
use crate::ecc::keccak256;
use crate::error::Error;
use crate::error::Result;

/// Hybrid logical version for LWW entry registers and element dots: the HLC pair
/// `(logical_time_ms, counter)` followed by the issuer, ordered lexicographically in field order.
///
/// `logical_time_ms` starts from the wall-clock millisecond observed at the storage-operation
/// boundary; a version issued after a floor that dominates it keeps the floor's millisecond and
/// raises `counter` instead (`EntryVersion::after`), so strict ascent never moves the millisecond
/// ahead of the clock (#1075). `actor` and `operation` make concurrent writes of different
/// issuers at one `(logical_time_ms, counter)` totally ordered without claiming recency.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct EntryVersion {
    /// Hybrid logical time in milliseconds.
    #[serde(alias = "epoch_ms")]
    pub logical_time_ms: u128,
    /// Logical counter within `logical_time_ms`: raised, instead of the millisecond, when a
    /// version is issued after a floor at the same or a later millisecond.
    pub counter: u32,
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
            counter: 0,
            actor,
            operation,
        }
    }

    /// The HLC issue step: `max(self, (l_f, c_f + 1, actor, operation))`. When `self`, the
    /// version at the issuer's clock `(now, 0, …)`, already exceeds `floor`, it is returned, so
    /// the millisecond follows the clock and the counter restarts at 0; only when `now ≤ l_f`
    /// does the counter step at `floor`'s millisecond.
    ///
    /// Laws: `v.after(f) > f`; `v.after(f).logical_time_ms = max(v.logical_time_ms, l_f)`, so
    /// strict ascent never advances the millisecond past the greater of the two; and
    /// `counter < 2³²` per millisecond, totally: at `c_f = u32::MAX` the step is refused with
    /// `EntryVersionAheadOfClock`, since the only way past it would advance the millisecond
    /// beyond the floor, which the caller's cap forbids; the writer is refused, never made to
    /// wait. The bound needs `2³²` versions issued after floors at one millisecond, more than
    /// `4·10¹²` writes a second, so no writer reaches it.
    pub(super) fn after(self, floor: Option<Self>) -> Result<Self> {
        if self.counter >= PROOF_CHUNK_SENTINEL_COUNTER {
            return Err(Error::EntryVersionAheadOfClock);
        }
        let Some(floor) = floor else {
            return Ok(self);
        };
        if self > floor {
            return Ok(self);
        }
        let counter = floor
            .counter
            .checked_add(1)
            .filter(|&counter| counter < PROOF_CHUNK_SENTINEL_COUNTER)
            .ok_or(Error::EntryVersionAheadOfClock)?;
        Ok(Self {
            logical_time_ms: floor.logical_time_ms,
            counter,
            actor: self.actor,
            operation: self.operation,
        })
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

/// Content address of one element: the Keccak-256 digest of its bytes.
///
/// A remove names the payload it removes by this digest rather than by its bytes, so a
/// tombstone costs a constant number of bytes whatever the payload size.
///
/// Law: `ElementDigest::of(u) = ElementDigest::of(v) ⟺ u = v`, up to Keccak-256 collision
/// resistance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ElementDigest(pub [u8; 32]);

#[cfg(test)]
thread_local! {
    /// Test instrumentation: the element digests computed on this thread, so a test can bound
    /// the hashing an operation performs.
    static DIGESTS_COMPUTED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test instrumentation: reset this thread's digest count.
#[cfg(test)]
pub(crate) fn reset_digests() {
    DIGESTS_COMPUTED.with(|computed| computed.set(0));
}

/// Test instrumentation: the element digests computed on this thread since the last reset.
#[cfg(test)]
pub(crate) fn digests_computed() -> usize {
    DIGESTS_COMPUTED.with(|computed| computed.get())
}

impl ElementDigest {
    /// The digest of the element `value`: the Keccak-256 of its bytes.
    pub fn of(value: &Bytes) -> Self {
        #[cfg(test)]
        DIGESTS_COMPUTED.with(|computed| computed.set(computed.get() + 1));
        Self(keccak256(value.as_ref()))
    }
}

/// Remove witness for one payload: it covers every add of the payload whose digest is `element`
/// at or below `dot` (see the module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntryTombstone {
    /// Digest of the removed payload.
    pub element: ElementDigest,
    /// Greatest add dot of the payload the remove covers.
    pub dot: EntryDot,
}

impl EntryTombstone {
    /// Remove witness covering the adds of `value` at or below `dot`.
    pub fn of(value: &Bytes, dot: EntryDot) -> Self {
        Self {
            element: ElementDigest::of(value),
            dot,
        }
    }
}

const SENTINEL_TIME_MS: u128 = u64::MAX as u128;
const CO_DOT_SENTINEL_COUNTER: u32 = u32::MAX;
const CO_REGISTER_SENTINEL_COUNTER: u32 = u32::MAX - 1;
pub(super) const PROOF_CHUNK_SENTINEL_COUNTER: u32 = u32::MAX - 2;

fn is_sentinel_version(version: EntryVersion, sentinel_counter: u32) -> bool {
    (version.logical_time_ms == SENTINEL_TIME_MS || version.logical_time_ms == u128::MAX)
        && version.counter == sentinel_counter
        && version.actor.is_default()
        && version.operation.is_default()
}

fn co_dot_sentinel_version() -> EntryVersion {
    EntryVersion {
        logical_time_ms: SENTINEL_TIME_MS,
        counter: CO_DOT_SENTINEL_COUNTER,
        actor: Did::default(),
        operation: Did::default(),
    }
}

fn co_register_sentinel_version() -> EntryVersion {
    EntryVersion {
        logical_time_ms: SENTINEL_TIME_MS,
        counter: CO_REGISTER_SENTINEL_COUNTER,
        actor: Did::default(),
        operation: Did::default(),
    }
}

fn data_proof_sentinel_version() -> EntryVersion {
    EntryVersion {
        logical_time_ms: SENTINEL_TIME_MS,
        counter: PROOF_CHUNK_SENTINEL_COUNTER,
        actor: Did::default(),
        operation: Did::default(),
    }
}

const DOT_CHUNK_BYTES: usize = 52;
const MAX_PROOF_BUNDLE_BYTES: usize = 1024 * 1024;

fn pack_proof_chunk(chunk: &[u8], chunk_index: usize) -> EntryDot {
    let mut buf = [0u8; DOT_CHUNK_BYTES];
    if let Some(prefix) = buf.get_mut(..chunk.len()) {
        prefix.copy_from_slice(chunk);
    }
    let logical_time_ms = u128::from(u64::from_be_bytes(
        buf.get(0..8)
            .and_then(|s| s.try_into().ok())
            .unwrap_or_default(),
    ));
    let counter = u32::from_be_bytes(
        buf.get(8..12)
            .and_then(|s| s.try_into().ok())
            .unwrap_or_default(),
    );
    let actor_bytes: [u8; 20] = buf
        .get(12..32)
        .and_then(|s| s.try_into().ok())
        .unwrap_or_default();
    let op_bytes: [u8; 20] = buf
        .get(32..52)
        .and_then(|s| s.try_into().ok())
        .unwrap_or_default();
    EntryDot {
        version: EntryVersion {
            logical_time_ms,
            counter,
            actor: ethereum_types::H160::from(actor_bytes).into(),
            operation: ethereum_types::H160::from(op_bytes).into(),
        },
        index: u32::try_from(chunk_index).unwrap_or(u32::MAX),
    }
}

fn unpack_proof_chunk(dot: EntryDot, expected_chunk_idx: usize) -> Result<[u8; DOT_CHUNK_BYTES]> {
    if u32::try_from(expected_chunk_idx).ok() != Some(dot.index) {
        return Err(Error::DataEntryWriteUnauthorized);
    }
    let low_time = u64::try_from(dot.version.logical_time_ms)
        .map_err(|_| Error::DataEntryWriteUnauthorized)?;
    let mut buf = [0u8; DOT_CHUNK_BYTES];
    if let Some(slice) = buf.get_mut(0..8) {
        slice.copy_from_slice(&low_time.to_be_bytes());
    }
    if let Some(slice) = buf.get_mut(8..12) {
        slice.copy_from_slice(&dot.version.counter.to_be_bytes());
    }
    if let Some(slice) = buf.get_mut(12..32) {
        slice.copy_from_slice(dot.version.actor.as_bytes());
    }
    if let Some(slice) = buf.get_mut(32..52) {
        slice.copy_from_slice(dot.version.operation.as_bytes());
    }
    Ok(buf)
}

/// CRDT metadata carried beside the entry payload.
///
/// `register` is the LWW reset floor used by overwrite. `dots` are per-element
/// add witnesses used by data/topic and relay element sets. `tombstones` is the
/// remove side: one covering remove per removed `(payload, actor)`. `co_dots`,
/// `co_registers`, and `proofs` preserve per-actor add witnesses, secondary
/// per-actor reset floors, and Swarm cryptographic author proofs, packed into
/// the `dots` sequence on the wire and on disk so the 3-field `EntryCrdt`
/// layout remains compatible with persisted `FileStorage` records.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "WireEntryCrdt", try_from = "WireEntryCrdt")]
pub struct EntryCrdt {
    /// Optional greatest LWW reset floor for the entry payload.
    pub register: Option<EntryVersion>,
    /// Per-element add dots. An element without one is given the dot of its index above the
    /// register floor (`EntryDot::for_index`).
    pub dots: Vec<EntryDot>,
    /// Covering removes, scoped per `(payload, actor)` for `Data` and per `payload` for `RelayMessage`.
    pub tombstones: Vec<EntryTombstone>,
    /// Additional per-actor add dots `(element_index, dot)` for elements concurrently held by
    /// multiple attributed writers.
    pub co_dots: Vec<(u32, EntryDot)>,
    /// Secondary per-actor reset floors when multiple attributed writers reset the carrier.
    pub co_registers: Vec<EntryVersion>,
    /// Cryptographic author proofs for Swarm-originated data mutations.
    pub proofs: Vec<super::provenance::SignedDataMutation>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename = "EntryCrdt")]
struct WireEntryCrdt {
    register: Option<EntryVersion>,
    dots: Vec<EntryDot>,
    tombstones: Vec<EntryTombstone>,
}

impl From<EntryCrdt> for WireEntryCrdt {
    fn from(crdt: EntryCrdt) -> Self {
        let co_dot_sentinel = co_dot_sentinel_version();
        let co_reg_sentinel = co_register_sentinel_version();
        let proof_sentinel = data_proof_sentinel_version();
        let mut dots = Vec::with_capacity(
            crdt.dots
                .len()
                .saturating_add(crdt.co_dots.len().saturating_mul(2))
                .saturating_add(crdt.co_registers.len().saturating_mul(2)),
        );
        dots.extend(crdt.dots);
        dots.extend(crdt.co_dots.into_iter().flat_map(|(elem_idx, dot)| {
            [
                EntryDot {
                    version: co_dot_sentinel,
                    index: elem_idx,
                },
                dot,
            ]
        }));
        dots.extend(crdt.co_registers.into_iter().flat_map(|version| {
            [
                EntryDot {
                    version: co_reg_sentinel,
                    index: 0,
                },
                EntryDot { version, index: 0 },
            ]
        }));
        if !crdt.proofs.is_empty() {
            if let Ok(encoded) = super::provenance::encode_proof_bundle(&crdt.proofs) {
                if encoded.len() <= MAX_PROOF_BUNDLE_BYTES {
                    if let Ok(byte_len) = u32::try_from(encoded.len()) {
                        dots.push(EntryDot {
                            version: proof_sentinel,
                            index: byte_len,
                        });
                        for (chunk_idx, chunk) in encoded.chunks(DOT_CHUNK_BYTES).enumerate() {
                            dots.push(pack_proof_chunk(chunk, chunk_idx));
                        }
                    }
                }
            }
        }
        Self {
            register: crdt.register,
            dots,
            tombstones: crdt.tombstones,
        }
    }
}

impl TryFrom<WireEntryCrdt> for EntryCrdt {
    type Error = Error;

    fn try_from(wire: WireEntryCrdt) -> Result<Self> {
        let mut dots = Vec::with_capacity(wire.dots.len());
        let mut co_dots = Vec::new();
        let mut co_registers = Vec::new();
        let mut proofs = Vec::new();
        let mut saw_proof_sentinel = false;
        let mut iter = wire.dots.into_iter();
        while let Some(dot) = iter.next() {
            if is_sentinel_version(dot.version, CO_DOT_SENTINEL_COUNTER) {
                let co_dot = iter.next().ok_or(Error::DataEntryWriteUnauthorized)?;
                co_dots.push((dot.index, co_dot));
            } else if is_sentinel_version(dot.version, CO_REGISTER_SENTINEL_COUNTER) {
                let reg_dot = iter.next().ok_or(Error::DataEntryWriteUnauthorized)?;
                co_registers.push(reg_dot.version);
            } else if is_sentinel_version(dot.version, PROOF_CHUNK_SENTINEL_COUNTER) {
                if saw_proof_sentinel {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                saw_proof_sentinel = true;
                let byte_len = dot.index as usize;
                if byte_len == 0 || byte_len > MAX_PROOF_BUNDLE_BYTES {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                let chunk_count = byte_len.div_ceil(DOT_CHUNK_BYTES);
                if chunk_count > iter.len() {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                let mut raw = Vec::with_capacity(chunk_count.saturating_mul(DOT_CHUNK_BYTES));
                for chunk_idx in 0..chunk_count {
                    let chunk_dot = iter.next().ok_or(Error::DataEntryWriteUnauthorized)?;
                    raw.extend_from_slice(&unpack_proof_chunk(chunk_dot, chunk_idx)?);
                }
                if raw
                    .get(byte_len..)
                    .is_none_or(|padding| padding.iter().any(|&b| b != 0))
                {
                    return Err(Error::DataEntryWriteUnauthorized);
                }
                raw.truncate(byte_len);
                proofs = super::provenance::decode_proof_bundle(&raw)?;
            } else {
                dots.push(dot);
            }
        }
        Ok(Self {
            register: wire.register,
            dots,
            tombstones: wire.tombstones,
            co_dots,
            co_registers,
            proofs,
        })
    }
}

impl EntryCrdt {
    pub(crate) fn encode_proof_dots(
        proofs: &[super::provenance::SignedDataMutation],
    ) -> Vec<EntryDot> {
        let mut dots = Vec::new();
        if !proofs.is_empty() {
            if let Ok(encoded) = super::provenance::encode_proof_bundle(proofs) {
                if encoded.len() <= MAX_PROOF_BUNDLE_BYTES {
                    if let Ok(byte_len) = u32::try_from(encoded.len()) {
                        dots.push(EntryDot {
                            version: data_proof_sentinel_version(),
                            index: byte_len,
                        });
                        for (chunk_idx, chunk) in encoded.chunks(DOT_CHUNK_BYTES).enumerate() {
                            dots.push(pack_proof_chunk(chunk, chunk_idx));
                        }
                    }
                }
            }
        }
        dots
    }

    pub(crate) fn decode_proof_dots(
        dots: Vec<EntryDot>,
    ) -> Vec<super::provenance::SignedDataMutation> {
        if dots.is_empty() {
            return Vec::new();
        }
        Self::try_from(WireEntryCrdt {
            register: None,
            dots,
            tombstones: Vec::new(),
        })
        .map(|crdt| crdt.proofs)
        .unwrap_or_default()
    }

    pub(super) fn has_write_witness(&self) -> bool {
        let witnessed = |version: &EntryVersion| {
            !version.actor.is_default() && (!version.operation.is_default() || version.counter != 0)
        };
        self.register.as_ref().is_some_and(witnessed)
            || self.co_registers.iter().any(witnessed)
            || self
                .dots
                .iter()
                .chain(self.co_dots.iter().map(|(_, dot)| dot))
                .any(|dot| witnessed(&dot.version))
    }

    /// Return the bottom floor used only to lift legacy payloads without dots.
    pub(super) fn legacy_floor(&self) -> EntryVersion {
        self.register.unwrap_or_default()
    }
}

/// Bounded LWW element set used by data topic buffers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataTopicBuffer {
    /// The greatest reset floor across overwrites.
    pub(super) register: Option<EntryVersion>,
    /// Per-actor reset floors keyed by attributed writer DID.
    pub(super) co_registers: BTreeMap<Did, EntryVersion>,
    /// Every element held and not covered by a remove, with the greatest dot that added it.
    pub(super) values: BTreeMap<Bytes, EntryDot>,
    /// Per-actor add dots for each held element, keyed by attributed writer DID.
    pub(super) co_values: BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
    /// Greatest remove per element digest across all actors.
    pub(super) removes: BTreeMap<ElementDigest, EntryDot>,
    /// Per-actor removes for each element digest, keyed by writer DID (`Did::default()` for unattributed).
    pub(super) co_removes: BTreeMap<ElementDigest, BTreeMap<Did, EntryDot>>,
    /// Cryptographic author proofs keyed by `(Register | Add | Remove, actor)`.
    pub(super) proofs:
        BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
}

/// Insert `value` at `key`, keeping the greater value when `key` is already present: the join of
/// one singleton into a pointwise-`max` map.
pub(super) fn insert_max<K: Ord, V: Ord>(map: &mut BTreeMap<K, V>, key: K, value: V) {
    if map.get(&key).is_none_or(|current| current.lt(&value)) {
        map.insert(key, value);
    }
}

fn has_register_proof(
    proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
    actor: Did,
    floor: EntryVersion,
) -> bool {
    !actor.is_default()
        && proofs
            .get(&super::provenance::DataProofKey::Register(actor))
            .is_some_and(|p| p.matches_register(floor))
}

fn has_add_proof(
    proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
    digest: ElementDigest,
    actor: Did,
    dot: EntryDot,
) -> bool {
    !actor.is_default()
        && proofs
            .get(&super::provenance::DataProofKey::Add(digest, actor))
            .is_some_and(|p| p.matches_add(digest, dot))
}

fn has_remove_proof(
    proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
    digest: ElementDigest,
    actor: Did,
    dot: EntryDot,
) -> bool {
    !actor.is_default()
        && proofs
            .get(&super::provenance::DataProofKey::Remove(digest, actor))
            .is_some_and(|p| p.matches_remove(digest, dot))
}

impl DataTopicBuffer {
    /// Record `version` as a reset floor, preferring signed floors over unsigned floors for the
    /// same attributed actor.
    pub(super) fn insert_register_with_proofs(
        register: &mut Option<EntryVersion>,
        co_registers: &mut BTreeMap<Did, EntryVersion>,
        proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
        version: EntryVersion,
    ) {
        if !version.actor.is_default() {
            let cand_signed = has_register_proof(proofs, version.actor, version);
            let replace = co_registers.get(&version.actor).is_none_or(|&curr| {
                let curr_signed = has_register_proof(proofs, version.actor, curr);
                (cand_signed, version) > (curr_signed, curr)
            });
            if replace {
                co_registers.insert(version.actor, version);
            }
        }
        *register = (*register).max(Some(version));
    }

    /// Record `(element, dot)` into `removes` and `co_removes`.
    pub(super) fn insert_remove(
        removes: &mut BTreeMap<ElementDigest, EntryDot>,
        co_removes: &mut BTreeMap<ElementDigest, BTreeMap<Did, EntryDot>>,
        element: ElementDigest,
        dot: EntryDot,
    ) {
        insert_max(
            co_removes.entry(element).or_default(),
            dot.version.actor,
            dot,
        );
        insert_max(removes, element, dot);
    }

    /// Record `(element, dot)` into `removes` and `co_removes`, preferring signed removes over
    /// unsigned removes for the same `(element, actor)`.
    pub(super) fn insert_remove_with_proofs(
        removes: &mut BTreeMap<ElementDigest, EntryDot>,
        co_removes: &mut BTreeMap<ElementDigest, BTreeMap<Did, EntryDot>>,
        proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
        element: ElementDigest,
        dot: EntryDot,
    ) {
        let actor = dot.version.actor;
        let cand_signed = has_remove_proof(proofs, element, actor, dot);
        let actors = co_removes.entry(element).or_default();
        let replace = actors.get(&actor).is_none_or(|&curr| {
            let curr_signed = has_remove_proof(proofs, element, actor, curr);
            (cand_signed, dot) > (curr_signed, curr)
        });
        if replace {
            actors.insert(actor, dot);
        }
        insert_max(removes, element, dot);
    }

    /// Record `(value, dot)` into `values` and `co_values`, preferring signed adds over unsigned
    /// adds for the same `(value, actor)` when `proofs` is non-empty.
    pub(super) fn insert_value_with_proofs(
        values: &mut BTreeMap<Bytes, EntryDot>,
        co_values: &mut BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
        proofs: &BTreeMap<super::provenance::DataProofKey, super::provenance::SignedDataMutation>,
        value: Bytes,
        dot: EntryDot,
    ) {
        let actor = dot.version.actor;
        if !actor.is_default() {
            let actors = co_values.entry(value.clone()).or_default();
            let replace = if proofs.is_empty() {
                actors.get(&actor).is_none_or(|&curr| dot > curr)
            } else {
                let digest = ElementDigest(keccak256(value.as_ref()));
                let cand_signed = has_add_proof(proofs, digest, actor, dot);
                actors.get(&actor).is_none_or(|&curr| {
                    let curr_signed = has_add_proof(proofs, digest, actor, curr);
                    (cand_signed, dot) > (curr_signed, curr)
                })
            };
            if replace {
                actors.insert(actor, dot);
            }
        }
        insert_max(values, value, dot);
    }

    /// Normalize a carrier: drop what the reset floor or a covering remove shadows.
    #[allow(dead_code)]
    pub(super) fn new(
        register: Option<EntryVersion>,
        values: BTreeMap<Bytes, EntryDot>,
        removes: BTreeMap<ElementDigest, EntryDot>,
    ) -> Self {
        let mut co_values = BTreeMap::<Bytes, BTreeMap<Did, EntryDot>>::new();
        for (value, dot) in &values {
            if !dot.version.actor.is_default() {
                insert_max(
                    co_values.entry(value.clone()).or_default(),
                    dot.version.actor,
                    *dot,
                );
            }
        }
        Self::with_co_values(register, values, co_values, removes)
    }

    /// Normalize a carrier with explicit per-actor add dots `co_values`.
    pub(super) fn with_co_values(
        register: Option<EntryVersion>,
        values: BTreeMap<Bytes, EntryDot>,
        co_values: BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
        removes: BTreeMap<ElementDigest, EntryDot>,
    ) -> Self {
        let mut co_registers = BTreeMap::new();
        if let Some(floor) = register {
            if !floor.actor.is_default() {
                co_registers.insert(floor.actor, floor);
            }
        }
        let mut co_removes = BTreeMap::<ElementDigest, BTreeMap<Did, EntryDot>>::new();
        for (&element, &dot) in &removes {
            insert_max(
                co_removes.entry(element).or_default(),
                dot.version.actor,
                dot,
            );
        }
        Self::normalize(
            register,
            co_registers,
            values,
            co_values,
            removes,
            co_removes,
            BTreeMap::new(),
        )
    }

    /// Normalize full per-actor state:
    /// - An attributed floor `co_registers[actor]` drops versions of that same `actor` below it
    ///   (and unattributed versions below it); an unattributed floor drops all versions below it.
    /// - An attributed remove `co_removes[digest][actor]` covers dots of that same `actor` at or
    ///   below it (and unattributed dots at or below it); an unattributed remove covers all dots
    ///   at or below it.
    pub(super) fn normalize(
        register: Option<EntryVersion>,
        mut co_registers: BTreeMap<Did, EntryVersion>,
        mut values: BTreeMap<Bytes, EntryDot>,
        mut co_values: BTreeMap<Bytes, BTreeMap<Did, EntryDot>>,
        mut removes: BTreeMap<ElementDigest, EntryDot>,
        mut co_removes: BTreeMap<ElementDigest, BTreeMap<Did, EntryDot>>,
        mut proofs: BTreeMap<
            super::provenance::DataProofKey,
            super::provenance::SignedDataMutation,
        >,
    ) -> Self {
        let mut scratch_register = register;
        if let Some(floor) = register {
            if !floor.actor.is_default() {
                Self::insert_register_with_proofs(
                    &mut scratch_register,
                    &mut co_registers,
                    &proofs,
                    floor,
                );
            }
        }
        let unattributed_floor = register.filter(|floor| floor.actor.is_default());

        for (value, dot) in &values {
            if !dot.version.actor.is_default() {
                let actor = dot.version.actor;
                let actors = co_values.entry(value.clone()).or_default();
                let replace = if proofs.is_empty() {
                    actors.get(&actor).is_none_or(|&curr| *dot > curr)
                } else {
                    let digest = ElementDigest(keccak256(value.as_ref()));
                    let cand_signed = has_add_proof(&proofs, digest, actor, *dot);
                    actors.get(&actor).is_none_or(|&curr| {
                        let curr_signed = has_add_proof(&proofs, digest, actor, curr);
                        (cand_signed, *dot) > (curr_signed, curr)
                    })
                };
                if replace {
                    actors.insert(actor, *dot);
                }
            }
        }
        for (&element, &dot) in &removes {
            Self::insert_remove_with_proofs(
                &mut BTreeMap::new(),
                &mut co_removes,
                &proofs,
                element,
                dot,
            );
        }

        if !proofs.is_empty() {
            let mut signed_actors = std::collections::BTreeSet::new();
            for (&actor, &floor) in &co_registers {
                if has_register_proof(&proofs, actor, floor) {
                    signed_actors.insert(actor);
                }
            }
            for (value, actors) in &co_values {
                let digest = ElementDigest(keccak256(value.as_ref()));
                for (&actor, &dot) in actors {
                    if has_add_proof(&proofs, digest, actor, dot) {
                        signed_actors.insert(actor);
                    }
                }
            }
            for (&digest, actors) in &co_removes {
                for (&actor, &dot) in actors {
                    if has_remove_proof(&proofs, digest, actor, dot) {
                        signed_actors.insert(actor);
                    }
                }
            }
            if !signed_actors.is_empty() {
                co_registers.retain(|&actor, &mut floor| {
                    !signed_actors.contains(&actor) || has_register_proof(&proofs, actor, floor)
                });
                for (value, actors) in &mut co_values {
                    let digest = ElementDigest(keccak256(value.as_ref()));
                    actors.retain(|&actor, &mut dot| {
                        !signed_actors.contains(&actor)
                            || has_add_proof(&proofs, digest, actor, dot)
                    });
                }
                for (&digest, actors) in &mut co_removes {
                    actors.retain(|&actor, &mut dot| {
                        !signed_actors.contains(&actor)
                            || has_remove_proof(&proofs, digest, actor, dot)
                    });
                }
            }
        }

        if let Some(u_floor) = unattributed_floor {
            co_registers.retain(|_, floor| *floor >= u_floor);
        }
        let max_floor = unattributed_floor
            .into_iter()
            .chain(co_registers.values().copied())
            .max();

        let kept_by_floors = |version: EntryVersion| -> bool {
            if unattributed_floor.is_some_and(|floor| version < floor) {
                return false;
            }
            if version.actor.is_default() {
                max_floor.is_none_or(|floor| version >= floor)
            } else {
                co_registers
                    .get(&version.actor)
                    .is_none_or(|floor| version >= *floor)
            }
        };

        if max_floor.is_some() {
            co_values.values_mut().for_each(|actors| {
                actors.retain(|_, dot| kept_by_floors(dot.version));
            });
            co_removes.values_mut().for_each(|actors| {
                actors.retain(|_, dot| kept_by_floors(dot.version));
            });
        }
        co_removes.retain(|_, actors| !actors.is_empty());
        removes.clear();
        for (&element, actors) in &co_removes {
            if let Some(max_dot) = actors.values().copied().max() {
                removes.insert(element, max_dot);
            }
        }

        values.retain(|value, primary_dot| {
            let actor_removes = (!co_removes.is_empty())
                .then(|| co_removes.get(&ElementDigest::of(value)))
                .flatten();
            let unattributed_remove =
                actor_removes.and_then(|map| map.get(&Did::default()).copied());
            let max_any_remove = actor_removes.and_then(|map| map.values().copied().max());

            if let Some(actors) = co_values.get_mut(value) {
                actors.retain(|actor, actor_dot| {
                    unattributed_remove.is_none_or(|rem| *actor_dot > rem)
                        && actor_removes
                            .and_then(|map| map.get(actor))
                            .is_none_or(|rem| *actor_dot > *rem)
                });
            }

            let unattributed_survives = primary_dot.version.actor.is_default()
                && kept_by_floors(primary_dot.version)
                && max_any_remove.is_none_or(|rem| *primary_dot > rem);

            let best_attributed = co_values
                .get(value)
                .and_then(|actors| actors.values().copied().max());
            let surviving = match (unattributed_survives, best_attributed) {
                (true, Some(attr)) => Some((*primary_dot).max(attr)),
                (true, None) => Some(*primary_dot),
                (false, attr) => attr,
            };
            match surviving {
                Some(best) => {
                    *primary_dot = best;
                    true
                }
                None => false,
            }
        });
        co_values.retain(|value, actors| values.contains_key(value) && !actors.is_empty());

        if !proofs.is_empty() {
            let live_adds = co_values
                .iter()
                .flat_map(|(value, actors)| {
                    let digest = ElementDigest(keccak256(value.as_ref()));
                    actors
                        .iter()
                        .map(move |(&actor, &dot)| ((digest, actor), dot))
                })
                .collect::<BTreeMap<_, _>>();
            proofs.retain(|key, mutation| match *key {
                super::provenance::DataProofKey::Register(actor) => co_registers
                    .get(&actor)
                    .is_some_and(|floor| mutation.matches_register(*floor)),
                super::provenance::DataProofKey::Add(digest, actor) => live_adds
                    .get(&(digest, actor))
                    .is_some_and(|dot| mutation.matches_add(digest, *dot)),
                super::provenance::DataProofKey::Remove(digest, actor) => co_removes
                    .get(&digest)
                    .and_then(|actors| actors.get(&actor))
                    .is_some_and(|dot| mutation.matches_remove(digest, *dot)),
            });
        }

        Self {
            register: max_floor,
            co_registers,
            values,
            co_values,
            removes,
            co_removes,
            proofs,
        }
    }

    /// Iterate over all surviving add dots held for `value` (`values` and `co_values`).
    #[allow(dead_code)]
    pub(super) fn dots_for<'a>(&'a self, value: &'a Bytes) -> impl Iterator<Item = EntryDot> + 'a {
        self.values.get(value).copied().into_iter().chain(
            self.co_values
                .get(value)
                .into_iter()
                .flat_map(|actors| actors.values().copied()),
        )
    }

    /// Record a covering remove for `value` across all held dots (used by relay inboxes and
    /// unattributed carrier removal).
    pub(super) fn remove(&mut self, value: &Bytes) {
        let removed_actors = self.co_values.remove(value);
        let primary = self.values.remove(value);
        if removed_actors.is_some() || primary.is_some() {
            let digest = ElementDigest::of(value);
            if let Some(actors) = removed_actors {
                for (_, dot) in actors {
                    Self::insert_remove(&mut self.removes, &mut self.co_removes, digest, dot);
                }
            }
            if let Some(dot) = primary {
                if dot.version.actor.is_default() || !self.co_removes.contains_key(&digest) {
                    Self::insert_remove(&mut self.removes, &mut self.co_removes, digest, dot);
                }
            }
        }
    }

    /// Record an actor-scoped covering remove for `value` on behalf of `actor`, leaving other
    /// co-authors' dots for `value` untouched (#1220).
    pub(super) fn remove_by(&mut self, value: &Bytes, actor: Did) {
        if actor.is_default() {
            self.remove(value);
            return;
        }
        let mut removed_dot = None;
        if let Some(actors) = self.co_values.get_mut(value) {
            removed_dot = actors.remove(&actor);
            if actors.is_empty() {
                self.co_values.remove(value);
            }
        }
        if let Some(dot) = removed_dot {
            let digest = ElementDigest::of(value);
            Self::insert_remove(&mut self.removes, &mut self.co_removes, digest, dot);
            self.proofs
                .remove(&super::provenance::DataProofKey::Add(digest, actor));
            match self
                .co_values
                .get(value)
                .and_then(|actors| actors.values().copied().max())
            {
                Some(remaining_max) => {
                    self.values.insert(value.clone(), remaining_max);
                }
                None => {
                    self.values.remove(value);
                }
            }
        } else if self
            .values
            .get(value)
            .is_some_and(|dot| dot.version.actor.is_default())
        {
            self.remove(value);
        }
    }
}

impl JoinSemilattice for DataTopicBuffer {
    fn join(mut self, other: Self) -> Self {
        for (key, proof) in other.proofs {
            insert_max(&mut self.proofs, key, proof);
        }
        let unattributed_register = self
            .register
            .filter(|floor| floor.actor.is_default())
            .max(other.register.filter(|floor| floor.actor.is_default()));
        if let Some(floor) = self.register.filter(|floor| !floor.actor.is_default()) {
            Self::insert_register_with_proofs(
                &mut self.register,
                &mut self.co_registers,
                &self.proofs,
                floor,
            );
        }
        if let Some(floor) = other.register.filter(|floor| !floor.actor.is_default()) {
            Self::insert_register_with_proofs(
                &mut self.register,
                &mut self.co_registers,
                &self.proofs,
                floor,
            );
        }
        for (_, floor) in other.co_registers {
            Self::insert_register_with_proofs(
                &mut self.register,
                &mut self.co_registers,
                &self.proofs,
                floor,
            );
        }
        for (element, dot) in other.removes {
            Self::insert_remove_with_proofs(
                &mut self.removes,
                &mut self.co_removes,
                &self.proofs,
                element,
                dot,
            );
        }
        for (element, actors) in other.co_removes {
            for (_, dot) in actors {
                Self::insert_remove_with_proofs(
                    &mut self.removes,
                    &mut self.co_removes,
                    &self.proofs,
                    element,
                    dot,
                );
            }
        }
        for (value, dot) in other.values {
            Self::insert_value_with_proofs(
                &mut self.values,
                &mut self.co_values,
                &self.proofs,
                value,
                dot,
            );
        }
        for (value, actors) in other.co_values {
            for (_, dot) in actors {
                Self::insert_value_with_proofs(
                    &mut self.values,
                    &mut self.co_values,
                    &self.proofs,
                    value.clone(),
                    dot,
                );
            }
        }
        Self::normalize(
            unattributed_register.or(self.register),
            self.co_registers,
            self.values,
            self.co_values,
            self.removes,
            self.co_removes,
            self.proofs,
        )
    }
}
