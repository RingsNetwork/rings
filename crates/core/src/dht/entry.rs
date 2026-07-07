#![warn(missing_docs)]
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
mod tests {
    use num_bigint::BigUint;

    use super::*;
    use crate::algebra::assert_join_semilattice_laws;
    use crate::algebra::assert_strong_eventual_consistency;
    use crate::ecc::SecretKey;
    use crate::message::Message;
    use crate::session::SessionSk;

    fn encoded(value: &str) -> Result<Encoded> {
        value.to_string().encode()
    }

    fn data_entry(topic: &str, value: &str) -> Result<Entry> {
        (topic.to_string(), encoded(value)?).try_into()
    }

    fn data_entry_from_values(topic: &str, values: Vec<String>) -> Result<Entry> {
        let data = values
            .into_iter()
            .map(|value| value.encode())
            .collect::<Result<Vec<_>>>()?;
        Ok(Entry::new(Entry::gen_did(topic)?, data, EntryKind::Data))
    }

    fn overflowing_data_entry(topic: &str, overflow: usize) -> Result<(Entry, usize)> {
        let incoming_count = ENTRY_DATA_MAX_LEN + overflow;
        let entry = data_entry_from_values(
            topic,
            (0..incoming_count)
                .map(|i| format!("incoming{i}"))
                .collect::<Vec<_>>(),
        )?;
        Ok((entry, incoming_count))
    }

    fn decode_entry_data(entry: &Entry) -> Result<Vec<String>> {
        entry
            .data
            .iter()
            .map(|item| item.decode())
            .collect::<Result<Vec<String>>>()
    }

    fn assert_entry_keeps_recent_overflow(
        entry: &Entry,
        incoming_count: usize,
        overflow: usize,
    ) -> Result<()> {
        assert_eq!(entry.data.len(), ENTRY_DATA_MAX_LEN);
        let decoded = decode_entry_data(entry)?;
        assert_eq!(decoded.first(), Some(&format!("incoming{overflow}")));
        assert_eq!(
            decoded.last(),
            Some(&format!("incoming{}", incoming_count - 1))
        );
        Ok(())
    }

    fn subring_entry(name: &str) -> Result<Entry> {
        let creator = Entry::gen_did("creator")?;
        Subring::new(name, creator)?.try_into()
    }

    fn actor() -> Did {
        Did::from(42u32)
    }

    fn version(counter: u32) -> EntryVersion {
        EntryVersion::new(
            u128::from(counter),
            Did::from(counter),
            Did::from(counter.saturating_add(1000)),
        )
    }

    fn data_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
        data_entry(topic, value)?.stamp_delta(version(counter))
    }

    fn overwrite_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
        data_entry(topic, value)?.stamp_overwrite(version(counter))
    }

    fn relay_delta(did: Did, value: &str, counter: u32) -> Result<Entry> {
        Entry::new(did, vec![encoded(value)?], EntryKind::RelayMessage)
            .stamp_delta(version(counter))
    }

    #[test]
    fn gset_satisfies_join_semilattice_laws() {
        let mut a = GSet::new();
        a.insert(Did::from(1u32));
        let mut b = GSet::new();
        b.insert(Did::from(2u32));
        let mut ab = GSet::new();
        ab.insert(Did::from(1u32));
        ab.insert(Did::from(2u32));

        assert_join_semilattice_laws(&[GSet::new(), a, b, ab]);
    }

    #[test]
    fn data_topic_buffer_satisfies_join_semilattice_laws() -> Result<()> {
        let carrier = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
            .join(data_delta("topic", "a", 1)?)?
            .join(data_delta("topic", "b", 2)?)?;
        let tombstoned_a = carrier
            .tombstone(data_delta("topic", "a", 1)?)?
            .topic_buffer()?;
        let samples = [
            Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data).topic_buffer()?,
            data_delta("topic", "a", 1)?.topic_buffer()?,
            data_delta("topic", "b", 2)?.topic_buffer()?,
            overwrite_delta("topic", "c", 3)?.topic_buffer()?,
            tombstoned_a,
        ];

        assert_join_semilattice_laws(&samples);
        Ok(())
    }

    #[test]
    fn relay_message_set_satisfies_join_semilattice_laws() -> Result<()> {
        let did = Did::from(10u32);
        let a = Entry::new(did, vec![encoded("a")?], EntryKind::RelayMessage)
            .stamp_delta(version(1))?
            .relay_set()?;
        let b = Entry::new(did, vec![encoded("b")?], EntryKind::RelayMessage)
            .stamp_delta(version(2))?
            .relay_set()?;
        let ab = Entry::new(did, vec![], EntryKind::RelayMessage)
            .join(relay_delta(did, "a", 1)?)?
            .join(relay_delta(did, "b", 2)?)?;
        let tombstoned_a = ab.tombstone(relay_delta(did, "a", 1)?)?.relay_set()?;

        assert_join_semilattice_laws(&[RelayMessageSet::default(), a, b, tombstoned_a]);
        Ok(())
    }

    #[test]
    fn entry_join_is_strongly_eventually_consistent_for_data_deltas() -> Result<()> {
        let base = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data);
        let deltas = [
            data_delta("topic", "a", 1)?,
            data_delta("topic", "b", 2)?,
            data_delta("topic", "a", 3)?,
        ];

        let forward = deltas
            .iter()
            .cloned()
            .try_fold(base.clone(), |acc, delta| acc.join(delta))?;
        let reverse = deltas
            .iter()
            .rev()
            .cloned()
            .try_fold(base.clone(), |acc, delta| acc.join(delta))?;
        let duplicated = deltas
            .iter()
            .cloned()
            .chain(deltas.iter().cloned())
            .try_fold(base, |acc, delta| acc.join(delta))?;

        assert_eq!(forward, reverse);
        assert_eq!(forward, duplicated);
        assert_eq!(decode_entry_data(&forward)?, vec![
            String::from("b"),
            String::from("a")
        ]);
        Ok(())
    }

    #[test]
    fn generic_sec_witness_accepts_data_topic_buffer_deltas() -> Result<()> {
        let base = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data).topic_buffer()?;
        let deltas = vec![
            data_delta("topic", "a", 1)?.topic_buffer()?,
            data_delta("topic", "b", 2)?.topic_buffer()?,
        ];

        assert_strong_eventual_consistency(base, &deltas);
        Ok(())
    }

    #[test]
    fn storage_normalization_uses_lattice_top_n_order() -> Result<()> {
        let incoming_count = ENTRY_DATA_MAX_LEN + 3;
        let mut entry = data_entry_from_values(
            "topic",
            (0..incoming_count)
                .map(|i| format!("incoming{i}"))
                .collect::<Vec<_>>(),
        )?;
        entry.crdt.dots = entry
            .data
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let counter = if index == 0 {
                    10_000
                } else {
                    u32::try_from(index).map_err(|_| Error::EntryDotIndexOutOfBounds { index })?
                };
                EntryDot::for_index(version(counter), index)
            })
            .collect::<Result<Vec<_>>>()?;

        let normalized = entry.try_into_storage_entry()?;
        let decoded = decode_entry_data(&normalized)?;

        assert_eq!(normalized.data.len(), ENTRY_DATA_MAX_LEN);
        assert_eq!(normalized.data.len(), normalized.crdt.dots.len());
        assert!(decoded.contains(&String::from("incoming0")));
        assert!(!decoded.contains(&String::from("incoming1")));
        assert!(!decoded.contains(&String::from("incoming2")));
        assert!(!decoded.contains(&String::from("incoming3")));
        Ok(())
    }

    #[test]
    fn storage_normalization_realigns_legacy_mismatched_dots() -> Result<()> {
        let mut entry = data_entry_from_values(
            "topic",
            (0..ENTRY_DATA_MAX_LEN + 2)
                .map(|i| format!("legacy{i}"))
                .collect::<Vec<_>>(),
        )?;
        entry.crdt.dots = vec![EntryDot::for_index(version(10_000), 0)?];

        let normalized = entry.try_into_storage_entry()?;

        assert_eq!(normalized.data.len(), ENTRY_DATA_MAX_LEN);
        assert_eq!(normalized.data.len(), normalized.crdt.dots.len());
        Ok(())
    }

    #[test]
    fn crdt_constructors_normalize_carrier_invariants() -> Result<()> {
        let register = version(10);
        let stale = encoded("stale")?;
        let live = encoded("live")?;
        let mut values = BTreeMap::new();
        values.insert(stale.clone(), EntryDot::for_index(version(1), 0)?);
        let live_dot = EntryDot::for_index(version(11), 0)?;
        values.insert(live.clone(), live_dot);

        let buffer = DataTopicBuffer::new(Some(register), values, BTreeSet::new());
        assert_eq!(buffer.values.len(), 1);
        assert!(buffer.values.contains_key(&live));

        let relay = RelayMessageSet::new(buffer, BTreeSet::from([live_dot]));
        assert!(relay.adds.values.is_empty());
        assert!(relay.removes.contains(&live_dot));
        Ok(())
    }

    #[test]
    fn overwrite_register_tiebreaker_converges_for_same_timestamp_actor() -> Result<()> {
        let did = Entry::gen_did("topic")?;
        let issuer = actor();
        let lower = Entry::new(did, vec![encoded("lower")?], EntryKind::Data)
            .stamp_overwrite(EntryVersion::new(1, issuer, Did::from(1u32)))?;
        let higher = Entry::new(did, vec![encoded("higher")?], EntryKind::Data)
            .stamp_overwrite(EntryVersion::new(1, issuer, Did::from(2u32)))?;
        let base = Entry::new(did, vec![], EntryKind::Data);

        let forward = base.clone().join(lower.clone())?.join(higher.clone())?;
        let reverse = base.join(higher)?.join(lower)?;

        assert_eq!(forward, reverse);
        assert_eq!(decode_entry_data(&forward)?, vec![String::from("higher")]);
        Ok(())
    }

    #[test]
    fn operation_digest_hashes_canonical_bytes_not_legacy_base58() -> Result<()> {
        let entry = data_entry("topic", "value")?;
        let digest = OperationDigest {
            kind: entry.kind,
            did: entry.did,
            data: &entry.data,
        };
        let bytes = bincode::serialize(&digest).map_err(Error::BincodeSerialize)?;

        let direct = Did::try_from(HashStr::from_bytes(&bytes))?;
        let legacy_encoded = bytes.encode()?;
        let legacy_base58 = Entry::gen_did(legacy_encoded.value())?;

        assert_eq!(entry.operation_digest()?, direct);
        assert_ne!(direct, legacy_base58);
        Ok(())
    }

    #[test]
    fn forwarded_overwrite_witness_is_not_reissued_after_local_floor() -> Result<()> {
        let current = overwrite_delta("topic", "current", 10)?;
        let stale_forwarded = overwrite_delta("topic", "stale", 1)?;

        let updated = current.overwrite(stale_forwarded, actor())?;

        assert_eq!(decode_entry_data(&updated)?, vec![String::from("current")]);
        Ok(())
    }

    #[test]
    fn overwrite_replaces_data_for_same_data_entry() -> Result<()> {
        let entry = data_entry("topic", "old")?;
        let other = data_entry("topic", "new")?;
        let updated = entry.overwrite(other, actor())?;
        assert_eq!(decode_entry_data(&updated)?, vec![String::from("new")]);
        Ok(())
    }

    #[test]
    fn overwrite_rejects_non_data_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let other = entry.clone();

        assert!(matches!(
            entry.overwrite(other, actor()),
            Err(Error::EntryNotOverwritable)
        ));
        Ok(())
    }

    #[test]
    fn overwrite_rejects_kind_mismatch() -> Result<()> {
        let entry = data_entry("topic", "old")?;
        let mut other = entry.clone();
        other.kind = EntryKind::RelayMessage;

        assert!(matches!(
            entry.overwrite(other, actor()),
            Err(Error::EntryKindNotEqual)
        ));
        Ok(())
    }

    #[test]
    fn overwrite_rejects_key_mismatch() -> Result<()> {
        let entry = data_entry("topic-a", "old")?;
        let other = data_entry("topic-b", "new")?;

        assert!(matches!(
            entry.overwrite(other, actor()),
            Err(Error::EntryDidNotEqual)
        ));
        Ok(())
    }

    #[test]
    fn overwrite_caps_payloads_larger_than_max_len() -> Result<()> {
        let overflow = 3;
        let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
        let entry = data_entry("topic", "base")?;
        let updated = entry.overwrite(incoming, actor())?;
        assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
    }

    #[test]
    fn extend_appends_data_for_same_entry() -> Result<()> {
        let entry = data_entry("topic", "first")?;
        let other = data_entry("topic", "second")?;
        let updated = entry.extend(other, actor())?;
        assert_eq!(decode_entry_data(&updated)?, vec![
            String::from("first"),
            String::from("second")
        ]);
        Ok(())
    }

    #[test]
    fn extend_trims_oldest_items_at_max_len() -> Result<()> {
        let mut entry = data_entry("topic", "test0")?;
        for i in 1..ENTRY_DATA_MAX_LEN {
            let data = format!("test{i}");
            let other = data_entry("topic", &data)?;
            entry = entry.extend(other, actor())?;
            assert_eq!(entry.data.len(), i + 1);
        }

        for i in ENTRY_DATA_MAX_LEN..ENTRY_DATA_MAX_LEN + 10 {
            let data = format!("test{i}");
            let other = data_entry("topic", &data)?;
            entry = entry.extend(other, actor())?;
            assert_eq!(entry.data.len(), ENTRY_DATA_MAX_LEN);
            let decoded = decode_entry_data(&entry)?;
            assert_eq!(
                decoded.first(),
                Some(&format!("test{}", i - ENTRY_DATA_MAX_LEN + 1))
            );
            assert_eq!(decoded.last(), Some(&data));
        }
        Ok(())
    }

    #[test]
    fn extend_caps_incoming_payloads_larger_than_max_len() -> Result<()> {
        let overflow = 3;
        let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
        let entry = data_entry("topic", "base")?;
        let updated = entry.extend(incoming, actor())?;
        assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
    }

    #[test]
    fn extend_rejects_non_data_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let other = entry.clone();

        assert!(matches!(
            entry.extend(other, actor()),
            Err(Error::EntryNotAppendable)
        ));
        Ok(())
    }

    #[test]
    fn touch_moves_existing_items_to_end_once() -> Result<()> {
        let entry = data_entry("topic", "a")?
            .extend(data_entry("topic", "b")?, actor())?
            .extend(data_entry("topic", "c")?, actor())?;
        let touched = data_entry("topic", "b")?;
        let updated = entry.touch(touched, actor())?;
        assert_eq!(decode_entry_data(&updated)?, vec![
            String::from("a"),
            String::from("c"),
            String::from("b")
        ]);
        Ok(())
    }

    #[test]
    fn touch_trims_oldest_non_touched_items_at_max_len() -> Result<()> {
        let mut entry = data_entry("topic", "test0")?;
        for i in 1..ENTRY_DATA_MAX_LEN {
            entry = entry.extend(data_entry("topic", &format!("test{i}"))?, actor())?;
        }
        let updated = entry.touch(data_entry("topic", "test0")?, actor())?;
        assert_eq!(updated.data.len(), ENTRY_DATA_MAX_LEN);
        let decoded = decode_entry_data(&updated)?;
        assert_eq!(decoded.first(), Some(&String::from("test1")));
        assert_eq!(decoded.last(), Some(&String::from("test0")));
        Ok(())
    }

    #[test]
    fn relay_tombstone_removes_observed_message_by_join() -> Result<()> {
        let did = Did::from(30u32);
        let first = relay_delta(did, "first", 1)?;
        let second = relay_delta(did, "second", 2)?;
        let carrier = Entry::new(did, vec![], EntryKind::RelayMessage)
            .join(first.clone())?
            .join(second.clone())?;

        let removed = carrier.tombstone(first.clone())?;

        assert_eq!(decode_entry_data(&removed)?, vec![String::from("second")]);
        let joined_with_stale_add = removed.join(first)?;
        assert_eq!(decode_entry_data(&joined_with_stale_add)?, vec![
            String::from("second")
        ]);
        Ok(())
    }

    #[test]
    fn data_tombstone_removes_observed_payload_by_join() -> Result<()> {
        let first = data_delta("topic", "first", 1)?;
        let second = data_delta("topic", "second", 2)?;
        let carrier = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
            .join(first.clone())?
            .join(second.clone())?;

        let removed = carrier.tombstone(first.clone())?;

        assert_eq!(decode_entry_data(&removed)?, vec![String::from("second")]);
        let joined_with_stale_add = removed.join(first)?;
        assert_eq!(decode_entry_data(&joined_with_stale_add)?, vec![
            String::from("second")
        ]);
        Ok(())
    }

    #[test]
    fn tombstone_rejects_non_data_or_relay_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let other = entry.clone();

        assert!(matches!(
            entry.tombstone(other),
            Err(Error::EntryNotTombstonable)
        ));
        Ok(())
    }

    #[test]
    fn touch_caps_incoming_payloads_larger_than_max_len() -> Result<()> {
        let overflow = 3;
        let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
        let entry = data_entry("topic", "base")?;
        let updated = entry.touch(incoming, actor())?;
        assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
    }

    #[test]
    fn join_subring_adds_member_to_subring_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let member = Entry::gen_did("member")?;
        let updated = entry.join_subring(member)?;
        let subring = Subring::try_from(updated)?;
        assert_eq!(subring.finger.first(), Some(member));
        Ok(())
    }

    #[test]
    fn join_subring_rejects_non_subring_entry() -> Result<()> {
        let entry = data_entry("topic", "value")?;
        let member = Entry::gen_did("member")?;

        assert!(matches!(
            entry.join_subring(member),
            Err(Error::EntryNotJoinable)
        ));
        Ok(())
    }

    #[test]
    fn operation_default_entry_matches_operation_kind() -> Result<()> {
        let target = data_entry("topic", "value")?;
        let default = EntryOperation::Extend(target.clone()).gen_default_entry()?;
        assert_eq!(default.did, target.did);
        assert_eq!(default.kind, EntryKind::Data);
        assert!(default.data.is_empty());
        Ok(())
    }

    #[test]
    fn message_payload_entry_key_targets_successor_of_signer() -> Result<()> {
        let key = SecretKey::random();
        let session = SessionSk::new_with_seckey(&key)?;
        let signer: Did = key.address().into();
        let payload =
            MessagePayload::new_send(Message::custom(b"relay")?, &session, signer, signer)?;
        let entry = Entry::try_from(payload)?;
        let expected = BigUint::from(signer) + BigUint::from(1u16);
        assert_eq!(entry.did, expected.into());
        assert_eq!(entry.kind, EntryKind::RelayMessage);
        Ok(())
    }

    #[test]
    fn affine_preserves_payload_and_kind_while_rotating_keys() -> Result<()> {
        let entry = data_entry("topic", "value")?;
        let affined = entry.affine(3)?;
        assert_eq!(affined.len(), 3);
        for rotated in affined {
            assert_eq!(rotated.data, entry.data);
            assert_eq!(rotated.kind, entry.kind);
        }
        Ok(())
    }
}
