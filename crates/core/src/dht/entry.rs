#![deny(missing_docs)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use crate::algebra::JoinSemilattice;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::consts::RELAY_INBOX_MAX_LEN;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;

mod crdt;
pub(crate) mod inbox;
mod retention;

use crdt::insert_max;
pub use crdt::DataTopicBuffer;
pub use crdt::ElementDigest;
pub use crdt::EntryCrdt;
pub use crdt::EntryDot;
pub use crdt::EntryTombstone;
pub use crdt::EntryVersion;

/// DHT storage entry categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// Encoded data stored in DHT
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

    /// The replication a carrier of this kind actually gets when `requested` is configured: a
    /// relay inbox has one owner and is never replicated.
    pub const fn replication(self, requested: u16) -> u16 {
        match self {
            EntryKind::Data => requested,
            EntryKind::RelayMessage => 1,
        }
    }

    /// The greatest number of tombstones a carrier of this kind keeps. A data topic has no
    /// count cap: a capped tombstone could be dropped while its add is still inside its element
    /// horizon somewhere, and a stale replica would resurrect it; its tombstones are bounded by
    /// rate instead, since each one retires `max_lifetime_ms + TS_OFFSET_TOLERANCE_MS` after its
    /// dot (see the `retention` module). A relay inbox has one owner and one ack-gated
    /// relocation at a time, so a stale copy can only be transient and the newest
    /// [`RELAY_INBOX_MAX_LEN`] removals suffice to shadow it.
    pub const fn max_tombstones(self) -> Option<usize> {
        match self {
            EntryKind::Data => None,
            EntryKind::RelayMessage => Some(RELAY_INBOX_MAX_LEN),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryStampKind {
    Overwrite,
    Delta,
}

/// The write witness an [`EntryOperation`] must carry after stamping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryWitness {
    /// Per-element dots, plus a reset register for overwrites.
    Elements(EntryStampKind),
    /// A reference witness: the operation names existing dots or values and issues none.
    Reference,
}

// Canonical stamp input for EntryVersion.operation.
//
// This digest is an unreleased CRDT tie-break witness between nodes running the
// same code, not a stable storage key or cross-version protocol identifier.
#[derive(Serialize)]
struct OperationDigest<'a> {
    kind: EntryKind,
    did: Did,
    data: &'a [Encoded],
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
}

impl PlacedEntryOperation {
    /// Return the entry identity carried by this operation.
    pub fn entry_key(&self) -> Result<Did> {
        self.op.did()
    }

    /// Return whether `placement` is in this entry's affine replica set.
    pub fn placement_belongs_to_entry(&self, redundancy: u16) -> Result<bool> {
        let entry_key = self.entry_key()?;
        placement_belongs_to_entry_key(entry_key, self.placement, self.op.kind(), redundancy)
    }

    /// Enforce that `placement` belongs to the operation's entry.
    pub fn validate_placement(&self, redundancy: u16) -> Result<()> {
        if self.placement_belongs_to_entry(redundancy)? {
            return Ok(());
        }

        Err(Error::InvalidMessage(
            "placed entry operation targets a placement outside the entry's affine replica set"
                .to_string(),
        ))
    }
}

/// Whether `placement` lies in the affine replica set of a carrier of `kind` identified by
/// `entry_key`: the set has `kind.replication(redundancy)` keys, one for a relay inbox.
fn placement_belongs_to_entry_key(
    entry_key: Did,
    placement: Did,
    kind: EntryKind,
    redundancy: u16,
) -> Result<bool> {
    Ok(entry_key
        .rotate_affine(kind.replication(redundancy))?
        .contains(&placement))
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
/// (see the `retention` module and [`Entry::validate_admissible_at`]). The bound joins by
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
    /// The data entity of `Entry`, encoded by [Encoder].
    pub data: Vec<Encoded>,
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacedEntry {
    /// The key used to place this value in DHT storage.
    pub key: Did,
    /// The stored entry value.
    pub entry: Entry,
}

impl PlacedEntry {
    /// Pair an entry value with the key where it is stored.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self { key, entry }
    }

    /// Return whether `key` is in `entry.did`'s affine replica set.
    pub fn placement_belongs_to_entry(&self, redundancy: u16) -> Result<bool> {
        placement_belongs_to_entry_key(self.entry.did, self.key, self.entry.kind, redundancy)
    }

    /// Enforce that `key` belongs to `entry.did`'s affine replica set.
    pub fn validate_placement(&self, redundancy: u16) -> Result<()> {
        if self.placement_belongs_to_entry(redundancy)? {
            return Ok(());
        }

        Err(Error::InvalidMessage(
            "synced placed entry targets a placement outside the entry's affine replica set"
                .to_string(),
        ))
    }
}

/// Durable-storage acknowledgement for an entry hand-off delta.
///
/// `key` is the placement key updated by the receiver. `entry` is the copied
/// delta that the receiver joined into its local least upper bound. Before
/// deleting, the sender compares the copied value with its current local value,
/// both normalized and projected by [`Entry::retired_at`] at the sender's clock;
/// if the sender has observed any newer durable delta meanwhile, deletion is
/// skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedEntryAck {
    /// The placement key durably persisted by the sync receiver.
    pub key: Did,
    /// The copied value the sync receiver durably joined.
    pub entry: Entry,
}

impl SyncedEntryAck {
    /// Witness that `entry` was durably joined at `key`.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self { key, entry }
    }

    /// Returns whether this ack proves that `local` equals the copied value at the clock
    /// `now_ms`.
    ///
    /// Post: comparison is performed on storage canonical forms projected to the element
    /// horizon at `now_ms` ([`Entry::retired_at`]), so legacy entries without dots compare equal
    /// to the normalized value durably persisted by the receiver, and an element or remove that
    /// merely crossed its horizon between the copy and the ack is not mistaken for a newer
    /// write: the copy was projected at an earlier clock, and projecting it again at `now_ms`
    /// yields what `local` is when nothing was written meanwhile.
    pub fn confirms_local_value(&self, local: &Entry, now_ms: u128) -> Result<bool> {
        let copied = self.entry.clone().try_into_storage_entry()?;
        let local = local.clone().try_into_storage_entry()?;
        Ok(copied.retired_at(now_ms) == local.retired_at(now_ms))
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
    pub fn new(did: Did, data: Vec<Encoded>, kind: EntryKind) -> Self {
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
        let witness = self.witness();
        self.try_map_entry(|entry| {
            let entry = entry.ensure_lifetime_from(now_ms);
            match witness {
                EntryWitness::Elements(kind) => entry.ensure_stamp_after(now_ms, actor, None, kind),
                EntryWitness::Reference => Ok(entry),
            }
        })
    }

    /// The write witness each operation kind must carry.
    const fn witness(&self) -> EntryWitness {
        match self {
            EntryOperation::Overwrite(_) => EntryWitness::Elements(EntryStampKind::Overwrite),
            EntryOperation::Extend(_) => EntryWitness::Elements(EntryStampKind::Delta),
            EntryOperation::Tombstone(_) => EntryWitness::Reference,
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

impl TryFrom<(String, Encoded)> for Entry {
    type Error = Error;
    fn try_from((topic, e): (String, Encoded)) -> Result<Self> {
        Ok(Self::new(Self::gen_did(&topic)?, vec![e], EntryKind::Data))
    }
}

impl TryFrom<(String, String)> for Entry {
    type Error = Error;
    fn try_from((topic, s): (String, String)) -> Result<Self> {
        let encoded_message = s.encode()?;
        (topic, encoded_message).try_into()
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

    fn operation_digest(&self) -> Result<Did> {
        let digest = OperationDigest {
            kind: self.kind,
            did: self.did,
            data: &self.data,
        };
        let bytes = rings_codec::serialize(&digest).map_err(Error::CodecSerialize)?;
        Did::try_from(HashStr::from_bytes(&bytes))
    }

    fn issue_version_after(
        &self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
    ) -> Result<EntryVersion> {
        Ok(EntryVersion::new(now_ms, actor, self.operation_digest()?).after(floor))
    }

    fn ensure_stamp_after(
        self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
        kind: EntryStampKind,
    ) -> Result<Self> {
        match self.crdt.has_write_witness() {
            true => Ok(self),
            false => {
                let version = self.issue_version_after(now_ms, actor, floor)?;
                self.stamp(version, kind)
            }
        }
    }

    /// Every version this entry carries: element dots, tombstones, and the reset floor.
    fn versions(&self) -> impl Iterator<Item = EntryVersion> + '_ {
        self.crdt
            .dots
            .iter()
            .map(|dot| dot.version)
            .chain(
                self.crdt
                    .tombstones
                    .iter()
                    .map(|tombstone| tombstone.dot.version),
            )
            .chain(self.crdt.register)
    }

    fn max_observed_version(&self) -> Option<EntryVersion> {
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
    /// remove, each keyed by its greatest dot, and the register.
    fn raw_buffer(&self) -> Result<DataTopicBuffer> {
        let mut values = BTreeMap::new();
        for (index, value) in self.data.iter().cloned().enumerate() {
            insert_max(&mut values, value, self.dot_for_element(index)?);
        }
        let mut removes = BTreeMap::new();
        for tombstone in self.crdt.tombstones.iter() {
            insert_max(&mut removes, tombstone.element, tombstone.dot);
        }
        Ok(DataTopicBuffer {
            register: self.crdt.register,
            values,
            removes,
        })
    }

    /// This entry's normalized carrier state (see [`DataTopicBuffer::new`]).
    fn topic_buffer(&self) -> Result<DataTopicBuffer> {
        let DataTopicBuffer {
            register,
            values,
            removes,
        } = self.raw_buffer()?;
        Ok(DataTopicBuffer::new(register, values, removes))
    }

    /// Materialize a normalized buffer as an entry: elements in dot order under the count cap,
    /// removes in dot order under the kind's tombstone cap.
    ///
    /// Pre: `buffer` is normalized, so no element is below the register or covered by a remove,
    /// and no digest is computed here.
    fn materialize_elements(
        did: Did,
        kind: EntryKind,
        buffer: DataTopicBuffer,
        expires_at_ms: Option<u128>,
    ) -> Self {
        let DataTopicBuffer {
            register,
            values,
            removes,
        } = buffer;
        let mut visible = values.into_iter().collect::<Vec<_>>();
        visible.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
            left_dot
                .cmp(right_dot)
                .then_with(|| left_value.cmp(right_value))
        });
        let skip_count = visible.len().saturating_sub(kind.max_data_len());
        let visible = visible.into_iter().skip(skip_count).collect::<Vec<_>>();
        let (data, dots): (Vec<_>, Vec<_>) = visible.into_iter().unzip();
        let mut tombstones = removes
            .into_iter()
            .map(|(element, dot)| EntryTombstone { element, dot })
            .collect::<Vec<_>>();
        tombstones.sort_by(|left, right| {
            left.dot
                .cmp(&right.dot)
                .then_with(|| left.element.cmp(&right.element))
        });
        let tombstone_skip = kind
            .max_tombstones()
            .map_or(0, |cap| tombstones.len().saturating_sub(cap));

        Self {
            did,
            data,
            kind,
            crdt: EntryCrdt {
                register,
                dots,
                tombstones: tombstones.into_iter().skip(tombstone_skip).collect(),
            },
            expires_at_ms,
        }
    }

    fn materialize_topic_buffer(
        &self,
        buffer: DataTopicBuffer,
        expires_at_ms: Option<u128>,
    ) -> Self {
        Self::materialize_elements(self.did, self.kind, buffer, expires_at_ms)
    }

    /// Merge two entries from the same replicated carrier.
    ///
    /// Law: for a fixed `(did, kind)` carrier, this is the state-based CRDT
    /// join. Data entries are bounded LWW element sets with an LWW overwrite
    /// register; relay entries are two-phase sets whose remove side is carried
    /// by tombstones. The retention bound joins by `max`, so the product of the
    /// payload lattice and the bound lattice is again a join-semilattice.
    ///
    /// Both sides enter the join unnormalized and the result is normalized once: normalization
    /// only drops what the joined register and removes still drop, so this equals the join of
    /// the normalized sides, and each payload digest is computed once.
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
    ///
    /// Post: normalization uses the same carrier materialization as
    /// [`Self::join`]; there is no second cap strategy outside the CRDT.
    /// Post: `result.data.len() <= kind.max_data_len()`; when the cap binds, the oldest payloads
    /// are the ones dropped.
    /// Post: `result.data.len() == result.crdt.dots.len()` for Data and
    /// RelayMessage entries.
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
            EntryOperation::Tombstone(entry) => self.tombstone(entry),
        }
    }

    /// Overwrite current data with new data.
    ///
    /// Preservation: the replacement is represented as a CRDT join. A newly
    /// stamped overwrite carries a reset floor, and materialization keeps only
    /// dots at or after that floor, so older payload dots are removed without a
    /// non-monotone assignment.
    ///
    /// The handler of [EntryOperation::Overwrite].
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

    /// Add `other`'s payloads to this carrier: the element-set join for a data topic and for
    /// a relay inbox alike, so holding a message for an offline peer is one ordinary write.
    /// The handler of [EntryOperation::Extend].
    pub fn extend(&self, now_ms: u128, other: Self, actor: Did) -> Result<Self> {
        self.join(other.ensure_stamp_after(
            now_ms,
            actor,
            self.max_observed_version(),
            EntryStampKind::Delta,
        )?)
    }

    /// Remove observed data or relay-message payloads.
    ///
    /// Pre: `self` and `other` are the same data or relay-message carrier.
    /// Post: every removed payload is represented by a covering remove at the dot this carrier
    /// holds for it, which also covers every earlier dot of the payload, including dots the
    /// carrier has already forgotten under a later one; so no future join with a stale add
    /// replica can resurrect it (#874). The carrier keeps its own retention bound: retention is
    /// refreshed by what is held, never by a removal, so a drained carrier expires when its last
    /// hold would have.
    pub fn tombstone(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;

        let expires_at_ms = self.expires_at_ms;
        let target_values = other.data.into_iter().collect::<BTreeSet<_>>();
        let target_dots = other.crdt.dots.into_iter().collect::<BTreeSet<_>>();
        let has_dot_witness = !target_dots.is_empty();

        let mut buffer = self.topic_buffer()?;
        let removed = buffer
            .values
            .iter()
            .filter(|(value, dot)| match has_dot_witness {
                true => target_dots.contains(*dot),
                false => target_values.contains(*value),
            })
            .map(|(value, _)| value.clone())
            .collect::<Vec<_>>();
        for value in removed.iter() {
            buffer.remove(value);
        }
        Ok(self.materialize_topic_buffer(buffer, expires_at_ms))
    }
}

#[cfg(test)]
mod test_entry;
#[cfg(test)]
mod test_horizon_model;
#[cfg(test)]
mod test_inbox;
