#![deny(missing_docs)]
use std::collections::BTreeMap;
use std::str::FromStr;

use bytes::Bytes;
use serde::Deserialize;
use serde::Serialize;

use crate::algebra::JoinSemilattice;
use crate::consts::DATA_TOMBSTONE_MAX_LEN;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;

mod crdt;
pub(crate) mod inbox;
mod provenance;
mod retention;

#[cfg(test)]
pub(crate) use crdt::digests_computed;
use crdt::insert_max;
#[cfg(test)]
pub(crate) use crdt::reset_digests;
pub use crdt::DataTopicBuffer;
pub use crdt::ElementDigest;
pub use crdt::EntryCrdt;
pub use crdt::EntryDot;
pub use crdt::EntryTombstone;
pub use crdt::EntryVersion;
use provenance::materialize_data_elements;
pub(crate) use provenance::DataProofKey;
use provenance::RetractionWitness;
pub use provenance::SignedDataMutation;
pub(crate) use retention::AdmittedEntry;

/// DHT storage entry categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum EntryKind {
    /// Application elements stored in the DHT, each as its bytes.
    Data,
    /// A relay inbox: messages held for an offline peer (see the `inbox` module).
    RelayMessage,
}

impl EntryKind {
    /// The greatest number of visible elements a carrier of this kind keeps; when the cap binds,
    /// the oldest elements are the ones dropped.
    pub const fn max_data_len(self) -> usize {
        match self {
            EntryKind::Data => ENTRY_DATA_MAX_LEN,
            EntryKind::RelayMessage => RELAY_INBOX_MAX_LEN,
        }
    }

    /// Whether this kind is a relay inbox.
    pub const fn is_relay_inbox(self) -> bool {
        matches!(self, EntryKind::RelayMessage)
    }

    /// The affine placements a carrier of this kind gets when `requested` is configured: a relay
    /// inbox has one placement, its key, and is replicated to the members of that key instead
    /// (`dht::relay_holder`, #777), never to affine rotations.
    pub const fn replication(self, requested: u16) -> u16 {
        match self {
            EntryKind::Data => requested,
            EntryKind::RelayMessage => 1,
        }
    }

    /// How a carrier of this kind bounds its removes (removal-stability paper, Law 5 and
    /// Prop. 3).
    ///
    /// A data topic refuses past [`DATA_TOMBSTONE_MAX_LEN`] and never truncates: a dropped
    /// tombstone could be one whose add is still inside its element horizon at another replica,
    /// which would then resurrect it; a refused write drops nothing. A relay inbox keeps the
    /// newest [`RELAY_INBOX_MAX_LEN`] removals: a lagging member (#777) may still resurrect an
    /// element whose removal fell off the cap, and its redelivery is refused by the recipient's
    /// receiver window and retired again, so the cap costs a redelivery, never a duplicate
    /// dispatch.
    pub const fn tombstone_bound(self) -> TombstoneBound {
        match self {
            EntryKind::Data => TombstoneBound::Refuse(DATA_TOMBSTONE_MAX_LEN),
            EntryKind::RelayMessage => TombstoneBound::Truncate(RELAY_INBOX_MAX_LEN),
        }
    }
}

/// How a carrier bounds its removes: by refusing a write past the bound, or by keeping the newest
/// removes up to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TombstoneBound {
    /// A write whose result would hold more removes is refused, changing nothing.
    Refuse(usize),
    /// Only the newest removes, by dot, up to the bound are kept.
    Truncate(usize),
}

impl TombstoneBound {
    /// How many of `count` removes, oldest first, materialization drops: the excess over a
    /// truncating bound, and none under a refusing one, which [`Entry::check_tombstone_bound`]
    /// enforces instead.
    const fn truncated(self, count: usize) -> usize {
        match self {
            Self::Refuse(_) => 0,
            Self::Truncate(cap) => count.saturating_sub(cap),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryStampKind {
    Overwrite,
    Delta,
}

// Canonical stamp input for EntryVersion.operation.
//
// This digest is an unreleased CRDT tie-break witness between nodes running the
// same code, not a stable storage key or cross-version protocol identifier.
#[derive(Serialize)]
struct OperationDigest<'a> {
    kind: EntryKind,
    did: Did,
    actor: Did,
    data: &'a [Bytes],
}

/// Operations supported by a DHT storage entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryOperation {
    /// Create or update an [`Entry`].
    Overwrite(Entry),
    /// Add payloads to a data topic or a relay inbox.
    /// This operation will create an [`Entry`] if it does not exist.
    Extend(Entry),
    /// Remove observed data or relay-message payloads.
    ///
    /// The payload identifies the entry carrier and the values to
    /// remove. If CRDT dots are present, the payloads the receiver holds at those
    /// dots are removed; otherwise the receiver removes the payloads it holds
    /// with matching bytes. Each removal is a covering remove at the dot the
    /// receiver holds (see [`EntryTombstone`]).
    Tombstone(Entry),
}

/// A storage operation targeted at one concrete affine placement key.
///
/// Invariant: `placement` must be one of the affine replica keys derived from
/// the operation's entry DID under the replication its kind gets from the
/// receiver's configured storage redundancy (a relay inbox has one placement,
/// its DID). The sender may choose a replica from that set, but cannot choose
/// where the replica set itself lives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedEntryOperation {
    /// Placement key that must receive the operation.
    pub placement: Did,
    /// Operation to apply at `placement`.
    pub op: EntryOperation,
    /// Whether the owner that applies or refuses the operation at `placement` reports it to the
    /// operation's origin (`message::types::store_report`, #866).
    pub report: bool,
}

impl PlacedEntryOperation {
    /// Return the entry identity carried by this operation.
    pub fn entry_key(&self) -> Result<Did> {
        self.op.did()
    }

    /// Enforce that `placement` belongs to the operation's entry.
    pub fn validate_placement(&self, redundancy: u16) -> Result<()> {
        validate_affine_placement(
            self.entry_key()?,
            self.placement,
            self.op.kind(),
            redundancy,
            "placed entry operation",
        )
    }
}

/// Enforce that `placement` lies in the affine replica set of a carrier of `kind` identified
/// by `entry_key`: the set has `kind.replication(redundancy)` keys, one for a relay inbox. The
/// error names the rejected carrier as `what`.
fn validate_affine_placement(
    entry_key: Did,
    placement: Did,
    kind: EntryKind,
    redundancy: u16,
    what: &'static str,
) -> Result<()> {
    if entry_key
        .rotate_affine(kind.replication(redundancy))?
        .contains(&placement)
    {
        return Ok(());
    }

    Err(Error::InvalidMessage(format!(
        "{what} targets a placement outside the entry's affine replica set"
    )))
}

/// A DHT storage entry with an [`EntryKind`] and a ring key represented as [`Did`].
///
/// An [`Entry`] is data stored by Chord storage on a [`PeerRing`](super::PeerRing). It is not a
/// Chord node and does not participate in successor, predecessor, or finger-table
/// membership.
///
/// The [`Did`] of an [`Entry`] is in the following format:
/// * If kind value is [EntryKind::Data], it's sha1 of data topic.
/// * If kind value is [EntryKind::RelayMessage], it's the destination Did of the held
///   messages plus 1, the position just after the destination, which lies in the
///   destination's own storage interval once it is online (see the `inbox` module).
///
/// The kind is part of the carrier's storage identity: a data topic and a relay inbox at the
/// same position are distinct slots, so neither can shadow the other.
///
/// Retention: every entry accepted into storage carries a retention bound `expires_at_ms`,
/// stamped by the origin at the operation boundary and bounded by the receiver at admission
/// (see the `retention` module and `Entry::validate_admissible_at`). The bound joins by
/// `max`, so every accepted write extends the carrier's life to at least its own bound, and an
/// entry whose bound has elapsed (and that holds no remove still inside its horizon) is dropped
/// on the next read instead of being served or replicated. Inside a live data carrier, every
/// element expires individually at its dot's issue time plus the element horizon
/// ([`EntryKind::element_horizon_ms`], see [`Entry::retired_at`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The ring key of this entry. It has the same representation as a node DID, but a
    /// different domain meaning.
    pub did: Did,
    /// The elements of this carrier, each as its bytes: the application's own encoding of the
    /// value, carried and digested as is, never re-encoded as text.
    pub data: Vec<Bytes>,
    /// The type indicates how the data is encoded and how the Did is generated.
    pub kind: EntryKind,
    /// CRDT metadata that makes replicated merge a join-semilattice operation.
    #[serde(default)]
    pub crdt: EntryCrdt,
    /// Retention bound in milliseconds since the Unix epoch. `None` only before the operation
    /// boundary stamps it; a stored value without a bound is treated as not live.
    #[serde(default)]
    pub expires_at_ms: Option<u128>,
}

/// An [`Entry`] paired with its Chord placement key.
///
/// `key` is the DHT storage location. `entry.did` is the resource identity. These two
/// values may differ for redundant replicas.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacedEntry {
    /// The key used to place this value in DHT storage.
    pub key: Did,
    /// The stored entry value.
    pub entry: Entry,
    /// Local custody incarnation observed when this entry was queued for hand-off.
    #[serde(skip)]
    custody_incarnation: Option<u64>,
}

impl PartialEq for PlacedEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.entry == other.entry
    }
}

impl Eq for PlacedEntry {}

impl PlacedEntry {
    /// Pair an entry value with the key where it is stored.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self {
            key,
            entry,
            custody_incarnation: None,
        }
    }

    /// Attach the local custody incarnation observed when this entry was queued for hand-off.
    pub(crate) fn with_custody_incarnation(mut self, incarnation: u64) -> Self {
        self.custody_incarnation = Some(incarnation);
        self
    }

    /// The local custody incarnation observed when this entry was queued for hand-off, if any.
    pub(crate) const fn custody_incarnation(&self) -> Option<u64> {
        self.custody_incarnation
    }

    /// Enforce that `key` belongs to `entry.did`'s affine replica set.
    pub fn validate_placement(&self, redundancy: u16) -> Result<()> {
        validate_affine_placement(
            self.entry.did,
            self.key,
            self.entry.kind,
            redundancy,
            "synced placed entry",
        )
    }
}

/// Durable-storage acknowledgement for an entry hand-off delta.
///
/// `key` is the placement key updated by the receiver. `entry` is the copied
/// delta that the receiver joined into its local least upper bound. Before
/// deleting, the sender compares the copied value with its current local value,
/// both normalized and projected by [`Entry::retired_at`] at the sender's clock,
/// and verifies that the acknowledgement is bound to the current local custody
/// incarnation of that storage slot; if the sender has observed any newer durable
/// delta or custody re-acquisition meanwhile, deletion is skipped.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyncedEntryAck {
    /// The placement key durably persisted by the sync receiver.
    pub key: Did,
    /// The copied value the sync receiver durably joined.
    pub entry: Entry,
    /// Local custody incarnation that emitted the hand-off this acknowledgement confirms.
    #[serde(skip)]
    custody_incarnation: u64,
}

impl PartialEq for SyncedEntryAck {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.entry == other.entry
    }
}

impl Eq for SyncedEntryAck {}

impl SyncedEntryAck {
    /// Witness that `entry` was durably joined at `key`.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self {
            key,
            entry,
            custody_incarnation: 0,
        }
    }

    /// Bind this acknowledgement to the sender's local custody incarnation `incarnation`.
    pub(crate) fn with_custody_incarnation(mut self, incarnation: u64) -> Self {
        self.custody_incarnation = incarnation;
        self
    }

    /// The sender's local custody incarnation bound to this acknowledgement.
    pub(crate) const fn custody_incarnation(&self) -> u64 {
        self.custody_incarnation
    }

    /// Returns whether this ack proves that `local` equals the copied value at the clock
    /// `now_ms`.
    ///
    /// Pre: both the copied value and `local` are normalized, and `local` is already projected
    /// at `now_ms`: `local` is the live stored value a storage read returns, and the copy is
    /// admitted only equal to one the sender took from its own storage.
    /// Post: the comparison is on both values projected to the element horizon at `now_ms`
    /// ([`Entry::retired_at`]), so an element or remove that merely crossed its horizon between
    /// the copy and the ack is not mistaken for a newer write: the copy was projected at an
    /// earlier clock, and projecting it again at `now_ms` yields what `local` is when nothing
    /// was written meanwhile. It computes no digest.
    pub fn confirms_local_value(&self, local: &Entry, now_ms: u128) -> bool {
        self.entry.clone().retired_at(now_ms).eq(local)
    }
}

/// A lookup request for a concrete placement of an entry identity.
///
/// `resource` is `id(e)`. `placement` is one element of
/// `place(resource, REDUNDANT)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryLookupKey {
    /// Entry identity being searched.
    pub resource: Did,
    /// Placement key being interrogated.
    pub placement: Did,
}

impl EntryLookupKey {
    /// Pair an entry identity with one of its placement keys.
    pub fn new(resource: Did, placement: Did) -> Self {
        Self {
            resource,
            placement,
        }
    }
}

/// A placement key observed missing during lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlacementMiss {
    /// Placement key whose responsible owner returned `None`.
    pub key: Did,
    /// Owner that was responsible for `key` when the miss was observed.
    pub owner: Did,
}

impl PlacementMiss {
    /// Witness that `owner` was queried for `key` and did not have the entry.
    pub fn new(key: Did, owner: Did) -> Self {
        Self { key, owner }
    }
}

/// A successful lookup result plus the missing placements observed before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryLookupEvidence {
    /// Entry found by the lookup.
    pub entry: Entry,
    /// Placement misses observed as part of the same lookup.
    pub misses: Vec<PlacementMiss>,
}

impl EntryLookupEvidence {
    /// Construct lookup evidence.
    pub fn new(entry: Entry, misses: Vec<PlacementMiss>) -> Self {
        Self { entry, misses }
    }
}

impl Entry {
    /// Construct an entry with empty CRDT metadata.
    pub fn new(did: Did, data: Vec<Bytes>, kind: EntryKind) -> Self {
        Self {
            did,
            data,
            kind,
            crdt: EntryCrdt::default(),
            expires_at_ms: None,
        }
    }

    /// Generate did from topic.
    pub fn gen_did(topic: &str) -> Result<Did> {
        let hash: HashStr = topic.into();
        let did = Did::from_str(&hash.inner());
        tracing::debug!("gen_did: topic: {}, did: {:?}", topic, did);
        did
    }
}

impl EntryOperation {
    /// Return this operation with CRDT versions and the retention bound assigned at the
    /// operation boundary `now_ms`.
    ///
    /// Existing CRDT witnesses and an existing retention bound are preserved so forwarded
    /// operations keep the origin's dot/version and lifetime instead of being reissued by every
    /// routing hop.
    ///
    /// Post: every carried entry has `expires_at_ms = Some(_)`; an absent bound becomes
    /// `now_ms + kind.default_lifetime_ms()`.
    pub fn stamped(self, now_ms: u128, actor: Did) -> Result<Self> {
        self.stamped_after(now_ms, actor, None)
    }

    /// [`Self::stamped`], issuing any version it stamps strictly after `floor`: the issue law of
    /// a node that stamps its operations in order (#1075), `logical_time = max(now, floor + 1)`
    /// when `now` alone would not exceed `floor` (`EntryVersion::after`).
    ///
    /// Post: a version this call issues is greater than `floor`; an operation that already
    /// carries its witnesses is returned with them unchanged.
    pub fn stamped_after(
        self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
    ) -> Result<Self> {
        let stamp = self.stamp_kind();
        self.try_map_entry(|entry| {
            let entry = entry.ensure_lifetime_from(now_ms);
            match stamp {
                Some(kind) => entry.ensure_stamp_after(now_ms, actor, floor, kind),
                None => {
                    let mut entry = entry;
                    if !entry.crdt.has_write_witness() {
                        entry.crdt.dots.clear();
                    }
                    Ok(entry)
                }
            }
        })
    }

    /// The write witness each operation kind must carry: per-element dots of `kind` (plus a
    /// reset register for an overwrite), or `None` for a reference operation, which names
    /// existing dots or values and issues none.
    const fn stamp_kind(&self) -> Option<EntryStampKind> {
        match self {
            EntryOperation::Overwrite(_) => Some(EntryStampKind::Overwrite),
            EntryOperation::Extend(_) => Some(EntryStampKind::Delta),
            EntryOperation::Tombstone(_) => None,
        }
    }

    /// The entry this operation carries.
    pub fn entry(&self) -> &Entry {
        match self {
            EntryOperation::Overwrite(entry)
            | EntryOperation::Extend(entry)
            | EntryOperation::Tombstone(entry) => entry,
        }
    }

    /// Apply `f` to the carried entry, keeping the operation kind.
    fn try_map_entry(self, f: impl FnOnce(Entry) -> Result<Entry>) -> Result<Self> {
        Ok(match self {
            EntryOperation::Overwrite(entry) => EntryOperation::Overwrite(f(entry)?),
            EntryOperation::Extend(entry) => EntryOperation::Extend(f(entry)?),
            EntryOperation::Tombstone(entry) => EntryOperation::Tombstone(f(entry)?),
        })
    }

    /// Extract the did of target Entry.
    pub fn did(&self) -> Result<Did> {
        Ok(self.entry().did)
    }

    /// Extract the kind of target Entry.
    pub fn kind(&self) -> EntryKind {
        self.entry().kind
    }

    /// Generate a target Entry when it is not existed.
    pub fn gen_default_entry(&self) -> Result<Entry> {
        Ok(Entry::new(self.did()?, vec![], self.kind()))
    }
}

impl TryFrom<(String, Bytes)> for Entry {
    type Error = Error;
    /// The data-topic delta holding the one element `element` of `topic`.
    fn try_from((topic, element): (String, Bytes)) -> Result<Self> {
        Ok(Self::new(
            Self::gen_did(&topic)?,
            vec![element],
            EntryKind::Data,
        ))
    }
}

impl TryFrom<(String, String)> for Entry {
    type Error = Error;
    /// The data-topic delta holding the text `text` of `topic` as its UTF-8 bytes.
    fn try_from((topic, text): (String, String)) -> Result<Self> {
        (topic, Bytes::from(text)).try_into()
    }
}

impl Entry {
    fn with_element_dots(mut self, version: EntryVersion) -> Result<Self> {
        self.crdt.dots = self
            .data
            .iter()
            .enumerate()
            .map(|(index, _)| EntryDot::for_index(version, index))
            .collect::<Result<Vec<_>>>()?;
        self.crdt.co_dots.clear();
        self.crdt.co_registers.clear();
        self.crdt.proofs.clear();
        Ok(self)
    }

    fn stamp_overwrite(mut self, version: EntryVersion) -> Result<Self> {
        self.crdt.register = Some(version);
        self.with_element_dots(version)
    }

    fn stamp_delta(self, version: EntryVersion) -> Result<Self> {
        self.with_element_dots(version)
    }

    fn stamp(self, version: EntryVersion, kind: EntryStampKind) -> Result<Self> {
        match kind {
            EntryStampKind::Overwrite => self.stamp_overwrite(version),
            EntryStampKind::Delta => self.stamp_delta(version),
        }
    }

    pub(super) fn operation_digest_for(&self, actor: Did, data: &[Bytes]) -> Result<Did> {
        let digest = OperationDigest {
            kind: self.kind,
            did: self.did,
            actor,
            data,
        };
        let bytes = rings_codec::serialize(&digest).map_err(Error::CodecSerialize)?;
        Did::try_from(HashStr::from_bytes(&bytes))
    }

    fn operation_digest(&self, actor: Did) -> Result<Did> {
        self.operation_digest_for(actor, &self.data)
    }

    fn issue_version_after(
        &self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
    ) -> Result<EntryVersion> {
        EntryVersion::new(now_ms, actor, self.operation_digest(actor)?).after(floor)
    }

    fn attribute_unattributed_versions(&mut self, actor: Did) {
        if actor.is_default() {
            return;
        }
        if let Some(reg) = &mut self.crdt.register {
            if reg.actor.is_default() {
                reg.actor = actor;
            }
        }
        for reg in &mut self.crdt.co_registers {
            if reg.actor.is_default() {
                reg.actor = actor;
            }
        }
        for dot in &mut self.crdt.dots {
            if dot.version.actor.is_default() {
                dot.version.actor = actor;
            }
        }
        for (_, dot) in &mut self.crdt.co_dots {
            if dot.version.actor.is_default() {
                dot.version.actor = actor;
            }
        }
    }

    fn ensure_stamp_after(
        mut self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
        kind: EntryStampKind,
    ) -> Result<Self> {
        match self.crdt.has_write_witness() {
            true => {
                self.attribute_unattributed_versions(actor);
                Ok(self)
            }
            false => {
                let stray_register = (kind == EntryStampKind::Delta)
                    .then_some(self.crdt.register)
                    .flatten();
                let version = self.issue_version_after(now_ms, actor, floor)?;
                let mut stamped = self.stamp(version, kind)?;
                if let Some(mut reg) = stray_register {
                    if !actor.is_default() && reg.actor.is_default() {
                        reg.actor = actor;
                    }
                    stamped.crdt.register = Some(reg);
                }
                Ok(stamped)
            }
        }
    }

    /// Every version this entry carries: element dots, co-author dots, tombstones, and reset
    /// floors.
    pub(super) fn versions(&self) -> impl Iterator<Item = EntryVersion> + '_ {
        self.crdt
            .dots
            .iter()
            .map(|dot| dot.version)
            .chain(self.crdt.co_dots.iter().map(|(_, dot)| dot.version))
            .chain(
                self.crdt
                    .tombstones
                    .iter()
                    .map(|tombstone| tombstone.dot.version),
            )
            .chain(self.crdt.register)
            .chain(self.crdt.co_registers.iter().copied())
    }

    /// The greatest version this carrier holds: of a dot, a remove, or a register floor.
    pub(crate) fn max_observed_version(&self) -> Option<EntryVersion> {
        self.versions().max()
    }

    fn validate_same_carrier(&self, other: &Self) -> Result<()> {
        if !self.same_kind_as(other) {
            return Err(Error::EntryKindNotEqual);
        }
        if !self.same_key_as(other) {
            return Err(Error::EntryDidNotEqual);
        }
        Ok(())
    }

    fn dot_for_element(&self, index: usize) -> Result<EntryDot> {
        if let Some(dot) = self.crdt.dots.get(index).copied() {
            return Ok(dot);
        }
        EntryDot::for_index(self.crdt.legacy_floor(), index)
    }

    /// This entry's carrier state before normalization: every element with its dot and every
    /// remove, each keyed by its greatest dot, and the reset floors.
    fn raw_buffer(&self) -> Result<DataTopicBuffer> {
        let mut proofs = BTreeMap::new();
        for proof in &self.crdt.proofs {
            insert_max(&mut proofs, proof.key, proof.clone());
        }
        let mut register = None;
        let mut co_registers = BTreeMap::new();
        if let Some(floor) = self.crdt.register {
            DataTopicBuffer::insert_register_with_proofs(
                &mut register,
                &mut co_registers,
                &proofs,
                floor,
            );
        }
        for &floor in &self.crdt.co_registers {
            DataTopicBuffer::insert_register_with_proofs(
                &mut register,
                &mut co_registers,
                &proofs,
                floor,
            );
        }
        let mut values = BTreeMap::new();
        let mut co_values = BTreeMap::new();
        for (index, value) in self.data.iter().cloned().enumerate() {
            DataTopicBuffer::insert_value_with_proofs(
                &mut values,
                &mut co_values,
                &proofs,
                value,
                self.dot_for_element(index)?,
            );
        }
        for &(index, dot) in &self.crdt.co_dots {
            if let Some(value) = self.data.get(index as usize).cloned() {
                DataTopicBuffer::insert_value_with_proofs(
                    &mut values,
                    &mut co_values,
                    &proofs,
                    value,
                    dot,
                );
            }
        }
        let mut removes = BTreeMap::new();
        let mut co_removes = BTreeMap::new();
        for tombstone in self.crdt.tombstones.iter() {
            DataTopicBuffer::insert_remove_with_proofs(
                &mut removes,
                &mut co_removes,
                &proofs,
                tombstone.element,
                tombstone.dot,
            );
        }
        Ok(DataTopicBuffer {
            register,
            co_registers,
            values,
            co_values,
            removes,
            co_removes,
            proofs,
        })
    }

    /// This entry's normalized carrier state (see [`DataTopicBuffer::normalize`]).
    pub(super) fn topic_buffer(&self) -> Result<DataTopicBuffer> {
        let raw = self.raw_buffer()?;
        Ok(DataTopicBuffer::normalize(
            raw.register,
            raw.co_registers,
            raw.values,
            raw.co_values,
            raw.removes,
            raw.co_removes,
            raw.proofs,
        ))
    }

    /// Materialize a normalized buffer as an entry: elements in dot order under the count cap,
    /// removes in dot order, truncated only under a truncating tombstone bound.
    ///
    /// Pre: `buffer` is normalized, so no element is below its actor's register floor or covered
    /// by a matching remove, and no digest is computed here unless `proofs` is non-empty.
    fn materialize_topic_buffer(
        &self,
        buffer: DataTopicBuffer,
        expires_at_ms: Option<u128>,
    ) -> Self {
        let kind = self.kind;
        let DataTopicBuffer {
            register,
            co_registers,
            values,
            co_values,
            removes,
            co_removes,
            mut proofs,
        } = buffer;
        let mut co_registers = co_registers
            .into_values()
            .filter(|&version| Some(version) != register)
            .collect::<Vec<_>>();
        co_registers.sort();
        let (data, dots, co_dots) = match kind {
            EntryKind::Data => materialize_data_elements(values, co_values, kind.max_data_len()),
            EntryKind::RelayMessage => {
                let mut visible = values.into_iter().collect::<Vec<_>>();
                visible.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
                    left_dot
                        .cmp(right_dot)
                        .then_with(|| left_value.cmp(right_value))
                });
                let skip_count = visible.len().saturating_sub(kind.max_data_len());
                let (data, dots) = visible.into_iter().skip(skip_count).unzip();
                (data, dots, Vec::new())
            }
        };
        let mut tombstones = match kind {
            EntryKind::Data => co_removes
                .into_iter()
                .flat_map(|(element, actors)| {
                    actors
                        .into_values()
                        .map(move |dot| EntryTombstone { element, dot })
                })
                .collect::<Vec<_>>(),
            EntryKind::RelayMessage => removes
                .into_iter()
                .map(|(element, dot)| EntryTombstone { element, dot })
                .collect::<Vec<_>>(),
        };
        tombstones.sort_by(|left, right| {
            left.dot
                .cmp(&right.dot)
                .then_with(|| left.element.cmp(&right.element))
        });
        let tombstone_skip = kind.tombstone_bound().truncated(tombstones.len());
        let tombstones: Vec<EntryTombstone> = tombstones.into_iter().skip(tombstone_skip).collect();

        if !proofs.is_empty() {
            let mut surviving_adds = BTreeMap::new();
            for (index, value) in data.iter().enumerate() {
                let digest = ElementDigest(crate::ecc::keccak256(value.as_ref()));
                if let Some(dot) = dots.get(index) {
                    if !dot.version.actor.is_default() {
                        surviving_adds.insert((digest, dot.version.actor), *dot);
                    }
                }
                for &(co_idx, dot) in &co_dots {
                    if co_idx as usize == index && !dot.version.actor.is_default() {
                        surviving_adds.insert((digest, dot.version.actor), dot);
                    }
                }
            }
            proofs.retain(|key, proof| match *key {
                DataProofKey::Add(digest, actor) => surviving_adds
                    .get(&(digest, actor))
                    .is_some_and(|dot| proof.matches_add(digest, *dot)),
                DataProofKey::Register(_) => register
                    .into_iter()
                    .chain(co_registers.iter().copied())
                    .any(|floor| proof.matches_register(floor)),
                DataProofKey::Remove(_, _) => tombstones
                    .iter()
                    .any(|t| proof.matches_remove(t.element, t.dot)),
            });
        }

        Self {
            did: self.did,
            data,
            kind,
            crdt: EntryCrdt {
                register,
                dots,
                tombstones,
                co_dots,
                co_registers,
                proofs: proofs.into_values().collect(),
            },
            expires_at_ms,
        }
    }

    /// Merge two entries from the same replicated carrier.
    pub fn join(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;
        let expires_at_ms = self.joined_lifetime(&other);
        Ok(self
            .materialize_topic_buffer(self.raw_buffer()?.join(other.raw_buffer()?), expires_at_ms))
    }

    fn is_data_entry(&self) -> bool {
        !self.kind.is_relay_inbox()
    }

    fn same_kind_as(&self, other: &Self) -> bool {
        self.kind == other.kind
    }

    fn same_key_as(&self, other: &Self) -> bool {
        self.did == other.did
    }

    /// Normalize an entry immediately before it is persisted.
    pub fn try_into_storage_entry(self) -> Result<Self> {
        let buffer = self.topic_buffer()?;
        Ok(self.materialize_topic_buffer(buffer, self.expires_at_ms))
    }

    /// The entry point of [EntryOperation] at the operation-boundary time `now_ms`, which
    /// stamps any unstamped witness. Will dispatch to different operation handlers according to
    /// the variant.
    pub fn operate(&self, now_ms: u128, op: EntryOperation, actor: Did) -> Result<Self> {
        match op {
            EntryOperation::Overwrite(entry) => self.overwrite(now_ms, entry, actor),
            EntryOperation::Extend(entry) => self.extend(now_ms, entry, actor),
            EntryOperation::Tombstone(entry) => match self.kind {
                EntryKind::Data if !actor.is_default() => self.tombstone_by(entry, actor),
                EntryKind::Data | EntryKind::RelayMessage => self.tombstone(entry),
            },
        }
    }

    /// Overwrite current data with new data.
    pub fn overwrite(&self, now_ms: u128, other: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotOverwritable);
        }
        self.join(other.ensure_stamp_after(
            now_ms,
            actor,
            self.max_observed_version(),
            EntryStampKind::Overwrite,
        )?)
    }

    /// Add `other`'s payloads to this carrier.
    pub fn extend(&self, now_ms: u128, other: Self, actor: Did) -> Result<Self> {
        self.join(other.ensure_stamp_after(
            now_ms,
            actor,
            self.max_observed_version(),
            EntryStampKind::Delta,
        )?)
    }

    /// Remove observed data payloads authored by `actor`, leaving other co-authors' attributions
    /// intact (#1220).
    pub fn tombstone_by(&self, other: Self, actor: Did) -> Result<Self> {
        if actor.is_default() || !self.is_data_entry() {
            return self.tombstone(other);
        }
        self.validate_same_carrier(&other)?;
        let expires_at_ms = self.expires_at_ms;
        let witness = RetractionWitness::of(&other);
        let mut buffer = self.topic_buffer()?;
        let removed = buffer
            .values
            .iter()
            .filter_map(|(value, primary_dot)| match buffer.co_values.get(value) {
                Some(actors) => actors
                    .get(&actor)
                    .copied()
                    .filter(|actor_dot| witness.matches(value, actor_dot))
                    .map(|dot| (value.clone(), Some(dot))),
                None => (primary_dot.version.actor.is_default()
                    && witness.matches(value, primary_dot))
                .then(|| (value.clone(), None)),
            })
            .collect::<Vec<_>>();
        for (value, removed_dot) in removed {
            buffer.remove_by(&value, actor);
            if !other.crdt.proofs.is_empty() {
                let digest = ElementDigest(crate::ecc::keccak256(value.as_ref()));
                let key = DataProofKey::Remove(digest, actor);
                if let Some(exact) = other.crdt.proofs.iter().find(|p| p.key == key) {
                    insert_max(&mut buffer.proofs, key, exact.clone());
                } else if let Some(dot) = removed_dot {
                    if let Some(mut dot_proof) = other
                        .crdt
                        .proofs
                        .iter()
                        .find(|p| {
                            p.key == DataProofKey::Remove(ElementDigest([0u8; 32]), actor)
                                && p.witness_dot == Some(dot)
                        })
                        .cloned()
                    {
                        dot_proof.key = key;
                        insert_max(&mut buffer.proofs, key, dot_proof);
                    }
                }
            }
        }
        Ok(self.materialize_topic_buffer(buffer, expires_at_ms))
    }

    /// Remove observed data or relay-message payloads.
    pub fn tombstone(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;

        let expires_at_ms = self.expires_at_ms;
        let witness = RetractionWitness::of(&other);

        let mut buffer = self.topic_buffer()?;
        let removed = buffer
            .values
            .iter()
            .filter(|(value, dot)| {
                witness.matches(value, dot)
                    || buffer
                        .co_values
                        .get(*value)
                        .is_some_and(|actors| actors.values().any(|d| witness.matches(value, d)))
            })
            .map(|(value, _)| value.clone())
            .collect::<Vec<_>>();
        removed.iter().for_each(|value| {
            buffer.remove(value);
        });
        Ok(self.materialize_topic_buffer(buffer, expires_at_ms))
    }
}

#[cfg(test)]
mod test_entry;
#[cfg(test)]
mod test_horizon_model;
#[cfg(test)]
mod test_inbox;
