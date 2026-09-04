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

pub use crdt::DataTopicBuffer;
pub use crdt::EntryCrdt;
pub use crdt::EntryDot;
pub use crdt::EntryVersion;
pub use crdt::RelayMessageSet;

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

    /// The greatest number of tombstones a carrier of this kind keeps. A data topic keeps every
    /// tombstone below its reset floor's pruning; a relay inbox has one owner and one
    /// ack-gated relocation at a time, so a stale copy can only be transient and the newest
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
    /// A register floor only.
    Register,
    /// No witness: the operation names existing dots or values.
    None,
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
    /// Extend data to a Data kind [`Entry`] uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// This operation will create an [`Entry`] if it does not exist.
    Touch(Entry),
    /// Tombstone observed data or relay-message payloads in a two-phase set.
    ///
    /// The payload identifies the entry carrier and the values to
    /// remove. If CRDT dots are present, those dots are the remove witnesses;
    /// otherwise the receiver tombstones currently observed dots with matching
    /// payload bytes.
    Tombstone(Entry),
    /// Compact a Data kind entry after removing listed payload bytes.
    ///
    /// The receiver computes the compacted live set from its current local
    /// entry, not from a sender snapshot. This preserves concurrent live writes
    /// already observed by the storage owner. The operation carries one
    /// source-stamped register floor shared by every replica, so divergent
    /// storage owners stay join-compatible after compaction.
    CompactData(Entry),
}

/// A storage operation targeted at one concrete affine placement key.
///
/// Invariant: `placement` must be one of the affine replica keys derived from
/// the operation's entry DID under the receiver's configured storage
/// redundancy. The sender may choose a replica from that set, but cannot choose
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
        placement_belongs_to_entry_key(entry_key, self.placement, redundancy)
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

fn placement_belongs_to_entry_key(entry_key: Did, placement: Did, redundancy: u16) -> Result<bool> {
    Ok(entry_key.rotate_affine(redundancy)?.contains(&placement))
}

/// A DHT storage entry with an [`EntryKind`] and a ring key represented as [`Did`].
///
/// An [`Entry`] is data stored by [`ChordStorage`](super::ChordStorage). It is not a
/// Chord node and does not participate in successor, predecessor, or finger-table
/// membership.
///
/// The [`Did`] of an [`Entry`] is in the following format:
/// * If kind value is [EntryKind::Data], it's sha1 of data topic.
/// * If kind value is [EntryKind::RelayMessage], it's the destination Did of
///   message plus 1 (to ensure that the message is sent to the successor of destination),
///   thus while destination node going online, it will sync message from its successor.
///
/// Retention: every entry accepted into storage carries a retention bound `expires_at_ms`,
/// stamped by the origin at the operation boundary and bounded by the receiver at admission
/// (see the `retention` module and [`Entry::validate_admissible_at`]). The bound joins by
/// `max`, so every accepted write extends the carrier's life to at least its own bound, and an
/// entry whose bound has elapsed is dropped on the next read instead of being served or
/// replicated.
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
        placement_belongs_to_entry_key(self.entry.did, self.key, redundancy)
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
/// delta that the receiver joined into its local least upper bound. The sender
/// compares the storage-normalized ack value with its current local value
/// before deleting; if the sender has observed any newer durable delta
/// meanwhile, deletion is skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedEntryAck {
    /// The placement key durably persisted by the sync receiver.
    pub key: Did,
    /// The exact value durably persisted by the sync receiver.
    pub entry: Entry,
}

impl SyncedEntryAck {
    /// Witness that `entry` was durably joined at `key`.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self { key, entry }
    }

    /// Returns whether this ack proves that `local` equals the copied value.
    ///
    /// Post: comparison is performed on storage canonical forms, so legacy
    /// entries without dots compare equal to the normalized value durably
    /// persisted by the receiver.
    pub fn confirms_local_value(&self, local: &Entry) -> Result<bool> {
        Ok(self.entry.clone().try_into_storage_entry()?
            == local.clone().try_into_storage_entry()?)
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
                EntryWitness::Register => entry.ensure_overwrite_stamp_after(now_ms, actor, None),
                EntryWitness::None => Ok(entry),
            }
        })
    }

    /// The write witness each operation kind must carry.
    const fn witness(&self) -> EntryWitness {
        match self {
            EntryOperation::Overwrite(_) => EntryWitness::Elements(EntryStampKind::Overwrite),
            EntryOperation::Extend(_) | EntryOperation::Touch(_) => {
                EntryWitness::Elements(EntryStampKind::Delta)
            }
            EntryOperation::Tombstone(_) => EntryWitness::None,
            EntryOperation::CompactData(_) => EntryWitness::Register,
        }
    }

    /// The entry this operation carries.
    pub fn entry(&self) -> &Entry {
        match self {
            EntryOperation::Overwrite(entry)
            | EntryOperation::Extend(entry)
            | EntryOperation::Touch(entry)
            | EntryOperation::Tombstone(entry)
            | EntryOperation::CompactData(entry) => entry,
        }
    }

    /// Apply `f` to the carried entry, keeping the operation kind.
    fn try_map_entry(self, f: impl FnOnce(Entry) -> Result<Entry>) -> Result<Self> {
        Ok(match self {
            EntryOperation::Overwrite(entry) => EntryOperation::Overwrite(f(entry)?),
            EntryOperation::Extend(entry) => EntryOperation::Extend(f(entry)?),
            EntryOperation::Touch(entry) => EntryOperation::Touch(f(entry)?),
            EntryOperation::Tombstone(entry) => EntryOperation::Tombstone(f(entry)?),
            EntryOperation::CompactData(entry) => EntryOperation::CompactData(f(entry)?),
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
    pub fn gen_default_entry(self) -> Result<Entry> {
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

impl TryFrom<String> for Entry {
    type Error = Error;
    fn try_from(topic: String) -> Result<Self> {
        (topic.clone(), topic).try_into()
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

    fn ensure_overwrite_stamp_after(
        self,
        now_ms: u128,
        actor: Did,
        floor: Option<EntryVersion>,
    ) -> Result<Self> {
        match self.crdt.register.is_some() {
            true => Ok(self),
            false => {
                let version = self.issue_version_after(now_ms, actor, floor)?;
                self.stamp_overwrite(version)
            }
        }
    }

    /// Every version this entry carries: element dots, tombstones, and the reset floor.
    fn versions(&self) -> impl Iterator<Item = EntryVersion> + '_ {
        self.crdt
            .dots
            .iter()
            .map(|dot| dot.version)
            .chain(self.crdt.tombstones.iter().map(|dot| dot.version))
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

    fn topic_buffer(&self) -> Result<DataTopicBuffer> {
        let mut values = BTreeMap::new();
        for (index, value) in self.data.iter().cloned().enumerate() {
            let dot = self.dot_for_element(index)?;
            values
                .entry(value)
                .and_modify(|current: &mut EntryDot| {
                    *current = (*current).max(dot);
                })
                .or_insert(dot);
        }
        Ok(DataTopicBuffer::new(
            self.crdt.register,
            values,
            self.crdt.tombstones.iter().copied().collect(),
        ))
    }

    fn relay_set(&self) -> Result<RelayMessageSet> {
        Ok(RelayMessageSet::new(
            self.topic_buffer()?,
            self.crdt.tombstones.iter().copied().collect(),
        ))
    }

    fn materialize_elements(
        did: Did,
        kind: EntryKind,
        register: Option<EntryVersion>,
        elements: impl IntoIterator<Item = (Encoded, EntryDot)>,
        tombstones: BTreeSet<EntryDot>,
        expires_at_ms: Option<u128>,
    ) -> Self {
        let mut visible = elements
            .into_iter()
            .filter(|(_, dot)| {
                let visible_after_reset = register.is_none_or(|floor| dot.version >= floor);
                visible_after_reset && !tombstones.contains(dot)
            })
            .collect::<Vec<_>>();
        visible.sort_by(|(left_value, left_dot), (right_value, right_dot)| {
            left_dot
                .cmp(right_dot)
                .then_with(|| left_value.cmp(right_value))
        });
        let skip_count = visible.len().saturating_sub(kind.max_data_len());
        let visible = visible.into_iter().skip(skip_count).collect::<Vec<_>>();
        let (data, dots): (Vec<_>, Vec<_>) = visible.into_iter().unzip();
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
        Self::materialize_elements(
            self.did,
            self.kind,
            buffer.register,
            buffer.values,
            buffer.removes,
            expires_at_ms,
        )
    }

    fn materialize_relay_set(&self, set: RelayMessageSet, expires_at_ms: Option<u128>) -> Self {
        Self::materialize_elements(
            self.did,
            self.kind,
            set.adds.register,
            set.adds.values,
            set.removes,
            expires_at_ms,
        )
    }

    fn compacted_data_dot(floor: EntryVersion, value: &Encoded) -> Result<EntryDot> {
        let operation = Did::try_from(HashStr::from_bytes(value.value().as_bytes()))?;
        let version =
            EntryVersion::new(floor.logical_time_ms, floor.actor, operation).after(Some(floor));
        EntryDot::for_index(version, 0)
    }

    fn compact_data_element(
        floor: EntryVersion,
        removal_values: &BTreeSet<Encoded>,
        value: Encoded,
        dot: EntryDot,
    ) -> Result<Option<(Encoded, EntryDot)>> {
        match dot.version < floor {
            true if removal_values.contains(&value) => Ok(None),
            true => Self::compacted_data_dot(floor, &value).map(|dot| Some((value, dot))),
            false => Ok(Some((value, dot))),
        }
    }

    fn data_compaction_candidates(
        payload_order: &[Encoded],
        live_values: BTreeMap<Encoded, EntryDot>,
    ) -> Vec<(Encoded, EntryDot)> {
        let (ordered_values, remaining_values) = payload_order.iter().fold(
            (Vec::new(), live_values),
            |(mut ordered, mut remaining), value| {
                if let Some(dot) = remaining.remove(value) {
                    ordered.push((value.clone(), dot));
                }
                (ordered, remaining)
            },
        );
        ordered_values.into_iter().chain(remaining_values).collect()
    }

    fn compact_data_elements(
        floor: EntryVersion,
        removal_values: &BTreeSet<Encoded>,
        values: impl IntoIterator<Item = (Encoded, EntryDot)>,
    ) -> Result<Vec<(Encoded, EntryDot)>> {
        values.into_iter().try_fold(
            Vec::new(),
            |mut elements, (value, dot)| -> Result<Vec<(Encoded, EntryDot)>> {
                match Self::compact_data_element(floor, removal_values, value, dot)? {
                    Some(element) => {
                        elements.push(element);
                        Ok(elements)
                    }
                    None => Ok(elements),
                }
            },
        )
    }

    fn compact_data_output_floor(
        current_floor: Option<EntryVersion>,
        operation_floor: EntryVersion,
    ) -> EntryVersion {
        current_floor.map_or(operation_floor, |current| current.max(operation_floor))
    }

    fn compact_data_tombstones(
        floor: EntryVersion,
        tombstones: BTreeSet<EntryDot>,
    ) -> BTreeSet<EntryDot> {
        tombstones
            .into_iter()
            .filter(|dot| dot.version >= floor)
            .collect()
    }

    /// Merge two entries from the same replicated carrier.
    ///
    /// Law: for a fixed `(did, kind)` carrier, this is the state-based CRDT
    /// join. Data entries are bounded LWW element sets with an LWW overwrite
    /// register; relay entries are two-phase sets whose remove side is carried
    /// by tombstones. The retention bound joins by `max`, so the product of the
    /// payload lattice and the bound lattice is again a join-semilattice.
    pub fn join(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;
        let expires_at_ms = self.joined_lifetime(&other);
        match self.kind {
            EntryKind::Data => Ok(self.materialize_topic_buffer(
                self.topic_buffer()?.join(other.topic_buffer()?),
                expires_at_ms,
            )),
            EntryKind::RelayMessage => Ok(self
                .materialize_relay_set(self.relay_set()?.join(other.relay_set()?), expires_at_ms)),
        }
    }

    /// Affine Transport entry to a list of affined did
    pub fn affine(&self, scalar: u16) -> Result<Vec<Entry>> {
        Ok(self
            .did
            .rotate_affine(scalar)?
            .into_iter()
            .map(|did| self.clone_with_did(did))
            .collect())
    }

    /// Clone and setup with new DID
    pub fn clone_with_did(&self, did: Did) -> Self {
        let mut entry = self.clone();
        entry.did = did;
        entry
    }

    fn is_data_entry(&self) -> bool {
        self.kind == EntryKind::Data
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
        match self.kind {
            EntryKind::Data => {
                let buffer = self.topic_buffer()?;
                Ok(self.materialize_topic_buffer(buffer, self.expires_at_ms))
            }
            EntryKind::RelayMessage => {
                let set = self.relay_set()?;
                Ok(self.materialize_relay_set(set, self.expires_at_ms))
            }
        }
    }

    /// The entry point of [EntryOperation] at the operation-boundary time `now_ms`, which
    /// stamps any unstamped witness. Will dispatch to different operation handlers according to
    /// the variant.
    pub fn operate(&self, now_ms: u128, op: EntryOperation, actor: Did) -> Result<Self> {
        match op {
            EntryOperation::Overwrite(entry) => self.overwrite(now_ms, entry, actor),
            EntryOperation::Extend(entry) => self.extend(now_ms, entry, actor),
            EntryOperation::Touch(entry) => self.touch(now_ms, entry, actor),
            EntryOperation::Tombstone(entry) => self.tombstone(entry),
            EntryOperation::CompactData(entry) => self.compact_data(now_ms, entry, actor),
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

    /// This method is used to extend data to a Data kind [`Entry`] uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// The handler of [EntryOperation::Touch].
    pub fn touch(&self, now_ms: u128, other: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotAppendable);
        }
        self.join(other.ensure_stamp_after(
            now_ms,
            actor,
            self.max_observed_version(),
            EntryStampKind::Delta,
        )?)
    }

    /// Tombstone observed data or relay-message payloads.
    ///
    /// Pre: `self` and `other` are the same data or relay-message carrier.
    /// Post: every removed payload is represented by an add-dot tombstone, so
    /// future joins with stale add replicas cannot resurrect it.
    pub fn tombstone(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;

        let expires_at_ms = self.joined_lifetime(&other);
        let target_values = other.data.into_iter().collect::<BTreeSet<_>>();
        let target_dots = other.crdt.dots.into_iter().collect::<BTreeSet<_>>();
        let has_dot_witness = !target_dots.is_empty();

        match self.kind {
            EntryKind::Data => {
                let mut buffer = self.topic_buffer()?;
                for (value, dot) in &buffer.values {
                    if target_dots.contains(dot)
                        || (!has_dot_witness && target_values.contains(value))
                    {
                        buffer.removes.insert(*dot);
                    }
                }
                Ok(self.materialize_topic_buffer(buffer, expires_at_ms))
            }
            EntryKind::RelayMessage => {
                let mut set = self.relay_set()?;
                for (value, dot) in &set.adds.values {
                    if target_dots.contains(dot)
                        || (!has_dot_witness && target_values.contains(value))
                    {
                        set.removes.insert(*dot);
                    }
                }
                Ok(self.materialize_relay_set(set, expires_at_ms))
            }
        }
    }

    /// Compact a data topic using the receiver's current visible payloads.
    ///
    /// Pre: `removals` names the same data topic as `self`; a relay inbox is never compacted
    /// by a reset floor, its removals are per-dot tombstones issued by its recipient.
    /// Post: every current visible payload not listed in `removals` is preserved
    /// under the greatest observed register floor, and older tombstone metadata
    /// is pruned by that floor.
    pub fn compact_data(&self, now_ms: u128, removals: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::RelayInboxOperationNotAllowed);
        }
        let removals =
            removals.ensure_overwrite_stamp_after(now_ms, actor, self.max_observed_version())?;
        self.validate_same_carrier(&removals)?;
        let expires_at_ms = self.joined_lifetime(&removals);
        let floor = removals.crdt.register.ok_or_else(|| {
            Error::InvalidMessage("compact data operation has no register floor".to_string())
        })?;
        let removal_values = removals.data.into_iter().collect::<BTreeSet<_>>();
        let buffer = self.topic_buffer()?;
        let (register, values, removes) = (buffer.register, buffer.values, buffer.removes);
        let output_floor = Self::compact_data_output_floor(register, floor);
        let elements = Self::compact_data_elements(
            floor,
            &removal_values,
            Self::data_compaction_candidates(&self.data, values),
        )?;
        let tombstones = Self::compact_data_tombstones(output_floor, removes);
        Ok(Self::materialize_elements(
            self.did,
            self.kind,
            Some(output_floor),
            elements,
            tombstones,
            expires_at_ms,
        ))
    }
}

#[cfg(test)]
mod test_entry;
#[cfg(test)]
mod test_inbox;
