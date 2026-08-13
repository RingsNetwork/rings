#![deny(missing_docs)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use super::subring::Subring;
use crate::algebra::JoinSemilattice;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;

mod crdt;

pub use crdt::DataTopicBuffer;
pub use crdt::EntryCrdt;
pub use crdt::EntryDot;
pub use crdt::EntryVersion;
pub use crdt::GSet;
pub use crdt::RelayMessageSet;
pub use crdt::SubringMemberSet;

/// DHT storage entry categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    /// Encoded data stored in DHT
    Data,
    /// Finger table of a Subring
    Subring,
    /// A relayed but unreached message, which should be stored on
    /// the successor of the destination Did.
    RelayMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
    data: &'a [Encoded],
}

/// Operations supported by a DHT storage entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryOperation {
    /// Create or update an [`Entry`].
    Overwrite(Entry),
    /// Extend data to a Data kind [`Entry`].
    /// This operation will create an [`Entry`] if it does not exist.
    Extend(Entry),
    /// Extend data to a Data kind [`Entry`] uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// This operation will create an [`Entry`] if it does not exist.
    Touch(Entry),
    /// Join subring.
    JoinSubring(String, Did),
    /// Tombstone observed data or relay-message payloads in a two-phase set.
    ///
    /// The payload identifies the entry carrier and the values to
    /// remove. If CRDT dots are present, those dots are the remove witnesses;
    /// otherwise the receiver tombstones currently observed dots with matching
    /// payload bytes.
    Tombstone(Entry),
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
/// * If kind value is [EntryKind::Subring], it's sha1 of Subring name.
/// * If kind value is [EntryKind::RelayMessage], it's the destination Did of
///   message plus 1 (to ensure that the message is sent to the successor of destination),
///   thus while destination node going online, it will sync message from its successor.
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
    /// Return this operation with CRDT versions assigned at the operation boundary.
    ///
    /// Existing CRDT witnesses are preserved so forwarded operations keep the
    /// origin's dot/version instead of being reissued by every routing hop.
    pub fn stamped(self, actor: Did) -> Result<Self> {
        Ok(match self {
            EntryOperation::Overwrite(entry) => EntryOperation::Overwrite(
                entry.ensure_stamp_after(actor, None, EntryStampKind::Overwrite)?,
            ),
            EntryOperation::Extend(entry) => EntryOperation::Extend(entry.ensure_stamp_after(
                actor,
                None,
                EntryStampKind::Delta,
            )?),
            EntryOperation::Touch(entry) => EntryOperation::Touch(entry.ensure_stamp_after(
                actor,
                None,
                EntryStampKind::Delta,
            )?),
            EntryOperation::JoinSubring(name, did) => EntryOperation::JoinSubring(name, did),
            EntryOperation::Tombstone(entry) => EntryOperation::Tombstone(entry),
        })
    }

    /// Extract the did of target Entry.
    pub fn did(&self) -> Result<Did> {
        Ok(match self {
            EntryOperation::Overwrite(entry) => entry.did,
            EntryOperation::Extend(entry) => entry.did,
            EntryOperation::Touch(entry) => entry.did,
            EntryOperation::JoinSubring(name, _) => Entry::gen_did(name)?,
            EntryOperation::Tombstone(entry) => entry.did,
        })
    }

    /// Extract the kind of target Entry.
    pub fn kind(&self) -> EntryKind {
        match self {
            EntryOperation::Overwrite(entry) => entry.kind,
            EntryOperation::Extend(entry) => entry.kind,
            EntryOperation::Touch(entry) => entry.kind,
            EntryOperation::JoinSubring(..) => EntryKind::Subring,
            EntryOperation::Tombstone(entry) => entry.kind,
        }
    }

    /// Generate a target Entry when it is not existed.
    pub fn gen_default_entry(self) -> Result<Entry> {
        match self {
            EntryOperation::JoinSubring(name, did) => Subring::new(&name, did)?.try_into(),
            _ => Ok(Entry::new(self.did()?, vec![], self.kind())),
        }
    }
}

impl TryFrom<MessagePayload> for Entry {
    type Error = Error;
    fn try_from(msg: MessagePayload) -> Result<Self> {
        // Relay entries target the signer's successor on R = Z / 2^160, so the
        // `+ 1` intentionally wraps in the fixed-width DID ring.
        let did = msg.signer() + Did::from(1u32);
        let data = msg.encode()?;
        Ok(Self {
            did,
            data: vec![data],
            kind: EntryKind::RelayMessage,
            crdt: EntryCrdt::default(),
        })
    }
}

impl TryFrom<(String, Encoded)> for Entry {
    type Error = Error;
    fn try_from((topic, e): (String, Encoded)) -> Result<Self> {
        Ok(Self {
            did: Self::gen_did(&topic)?,
            data: vec![e],
            kind: EntryKind::Data,
            crdt: EntryCrdt::default(),
        })
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
        let bytes = bincode::serialize(&digest).map_err(Error::BincodeSerialize)?;
        Did::try_from(HashStr::from_bytes(&bytes))
    }

    fn issue_version_after(&self, actor: Did, floor: Option<EntryVersion>) -> Result<EntryVersion> {
        Ok(EntryVersion::issued_by(actor, self.operation_digest()?).after(floor))
    }

    fn ensure_stamp_after(
        self,
        actor: Did,
        floor: Option<EntryVersion>,
        kind: EntryStampKind,
    ) -> Result<Self> {
        if self.crdt.has_write_witness() {
            return Ok(self);
        }
        let version = self.issue_version_after(actor, floor)?;
        self.stamp(version, kind)
    }

    fn max_observed_version(&self) -> Option<EntryVersion> {
        self.crdt
            .dots
            .iter()
            .map(|dot| dot.version)
            .chain(self.crdt.tombstones.iter().map(|dot| dot.version))
            .chain(self.crdt.register)
            .max()
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

    fn subring_member_set(&self) -> Result<SubringMemberSet> {
        let subring: Subring = self.clone().try_into()?;
        let mut members = SubringMemberSet::new();
        for member in subring.finger.list().iter().flatten().copied() {
            members.insert(member);
        }
        Ok(members)
    }

    fn materialize_elements(
        did: Did,
        kind: EntryKind,
        register: Option<EntryVersion>,
        elements: impl IntoIterator<Item = (Encoded, EntryDot)>,
        tombstones: BTreeSet<EntryDot>,
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
        let skip_count = visible.len().saturating_sub(ENTRY_DATA_MAX_LEN);
        let visible = visible.into_iter().skip(skip_count).collect::<Vec<_>>();
        let (data, dots): (Vec<_>, Vec<_>) = visible.into_iter().unzip();

        Self {
            did,
            data,
            kind,
            crdt: EntryCrdt {
                register,
                dots,
                tombstones: tombstones.into_iter().collect(),
            },
        }
    }

    fn materialize_topic_buffer(&self, buffer: DataTopicBuffer) -> Self {
        Self::materialize_elements(
            self.did,
            self.kind,
            buffer.register,
            buffer.values,
            buffer.removes,
        )
    }

    fn materialize_relay_set(&self, set: RelayMessageSet) -> Self {
        Self::materialize_elements(
            self.did,
            self.kind,
            set.adds.register,
            set.adds.values,
            set.removes,
        )
    }

    fn join_subring_entry(&self, other: &Self) -> Result<Self> {
        let members = self.subring_member_set()?.join(other.subring_member_set()?);
        let mut subring: Subring = self.clone().try_into()?;
        for member in members.iter().copied() {
            subring.finger.join(member);
        }
        let mut entry: Entry = subring.try_into()?;
        entry.crdt.register = self.crdt.register.max(other.crdt.register);
        Ok(entry)
    }

    /// Merge two entries from the same replicated carrier.
    ///
    /// Law: for a fixed `(did, kind)` carrier, this is the state-based CRDT
    /// join. Data entries are bounded LWW element sets with an LWW overwrite
    /// register; subring entries are grow-only member sets; relay entries are
    /// two-phase sets whose remove side is carried by tombstones.
    pub fn join(&self, other: Self) -> Result<Self> {
        self.validate_same_carrier(&other)?;
        match self.kind {
            EntryKind::Data => {
                Ok(self.materialize_topic_buffer(self.topic_buffer()?.join(other.topic_buffer()?)))
            }
            EntryKind::RelayMessage => {
                Ok(self.materialize_relay_set(self.relay_set()?.join(other.relay_set()?)))
            }
            EntryKind::Subring => self.join_subring_entry(&other),
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

    fn is_subring_entry(&self) -> bool {
        self.kind == EntryKind::Subring
    }

    fn is_relay_entry(&self) -> bool {
        self.kind == EntryKind::RelayMessage
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
    /// Post: `result.data.len() <= ENTRY_DATA_MAX_LEN`.
    /// Post: `result.data.len() == result.crdt.dots.len()` for Data and
    /// RelayMessage entries.
    pub fn try_into_storage_entry(self) -> Result<Self> {
        match self.kind {
            EntryKind::Data => {
                let buffer = self.topic_buffer()?;
                Ok(self.materialize_topic_buffer(buffer))
            }
            EntryKind::RelayMessage => {
                let set = self.relay_set()?;
                Ok(self.materialize_relay_set(set))
            }
            EntryKind::Subring => Ok(self),
        }
    }

    /// The entry point of [EntryOperation].
    /// Will dispatch to different operation handlers according to the variant.
    pub fn operate(&self, op: EntryOperation, actor: Did) -> Result<Self> {
        match op {
            EntryOperation::Overwrite(entry) => self.overwrite(entry, actor),
            EntryOperation::Extend(entry) => self.extend(entry, actor),
            EntryOperation::Touch(entry) => self.touch(entry, actor),
            EntryOperation::JoinSubring(_, did) => self.join_subring(did),
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
    pub fn overwrite(&self, other: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotOverwritable);
        }
        self.join(other.ensure_stamp_after(
            actor,
            self.max_observed_version(),
            EntryStampKind::Overwrite,
        )?)
    }

    /// This method is used to extend data to a Data kind [`Entry`].
    /// The handler of [EntryOperation::Extend].
    pub fn extend(&self, other: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotAppendable);
        }
        self.join(other.ensure_stamp_after(
            actor,
            self.max_observed_version(),
            EntryStampKind::Delta,
        )?)
    }

    /// This method is used to extend data to a Data kind [`Entry`] uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// The handler of [EntryOperation::Touch].
    pub fn touch(&self, other: Self, actor: Did) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotAppendable);
        }
        self.join(other.ensure_stamp_after(
            actor,
            self.max_observed_version(),
            EntryStampKind::Delta,
        )?)
    }

    /// This method is used to join a subring.
    /// The handler of [EntryOperation::JoinSubring].
    pub fn join_subring(&self, did: Did) -> Result<Self> {
        if !self.is_subring_entry() {
            return Err(Error::EntryNotJoinable);
        }

        let mut subring: Subring = self.clone().try_into()?;
        subring.finger.join(did);
        let other: Entry = subring.try_into()?;
        self.join(other)
    }

    /// Tombstone observed data or relay-message payloads.
    ///
    /// Pre: `self` and `other` are the same data or relay-message carrier.
    /// Post: every removed payload is represented by an add-dot tombstone, so
    /// future joins with stale add replicas cannot resurrect it.
    pub fn tombstone(&self, other: Self) -> Result<Self> {
        if !self.is_data_entry() && !self.is_relay_entry() {
            return Err(Error::EntryNotTombstonable);
        }
        self.validate_same_carrier(&other)?;

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
                Ok(self.materialize_topic_buffer(buffer))
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
                Ok(self.materialize_relay_set(set))
            }
            EntryKind::Subring => Err(Error::EntryNotTombstonable),
        }
    }
}

#[cfg(test)]
mod tests;
