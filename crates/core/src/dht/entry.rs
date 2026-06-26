#![warn(missing_docs)]
use std::str::FromStr;

use serde::Deserialize;
use serde::Serialize;

use super::subring::Subring;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::dht::Did;
use crate::ecc::HashStr;
use crate::error::Error;
use crate::error::Result;
use crate::message::Encoded;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::message::MessageVerificationExt;

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
}

/// Durable-storage acknowledgement for an entry hand-off.
///
/// `key` is the placement key durably persisted by the receiver. `entry` is the
/// exact value persisted there and therefore the equality witness used by the
/// sender before deleting its local copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncedEntryAck {
    /// The placement key durably persisted by the sync receiver.
    pub key: Did,
    /// The exact value durably persisted by the sync receiver.
    pub entry: Entry,
}

impl SyncedEntryAck {
    /// Witness that `entry` was durably persisted at `key`.
    pub fn new(key: Did, entry: Entry) -> Self {
        Self { key, entry }
    }

    /// Returns whether this ack proves that `local` equals the copied value.
    pub fn confirms_local_value(&self, local: &Entry) -> bool {
        &self.entry == local
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
    /// Generate did from topic.
    pub fn gen_did(topic: &str) -> Result<Did> {
        let hash: HashStr = topic.into();
        let did = Did::from_str(&hash.inner());
        tracing::debug!("gen_did: topic: {}, did: {:?}", topic, did);
        did
    }
}

impl EntryOperation {
    /// Extract the did of target Entry.
    pub fn did(&self) -> Result<Did> {
        Ok(match self {
            EntryOperation::Overwrite(entry) => entry.did,
            EntryOperation::Extend(entry) => entry.did,
            EntryOperation::Touch(entry) => entry.did,
            EntryOperation::JoinSubring(name, _) => Entry::gen_did(name)?,
        })
    }

    /// Extract the kind of target Entry.
    pub fn kind(&self) -> EntryKind {
        match self {
            EntryOperation::Overwrite(entry) => entry.kind,
            EntryOperation::Extend(entry) => entry.kind,
            EntryOperation::Touch(entry) => entry.kind,
            EntryOperation::JoinSubring(..) => EntryKind::Subring,
        }
    }

    /// Generate a target Entry when it is not existed.
    pub fn gen_default_entry(self) -> Result<Entry> {
        match self {
            EntryOperation::JoinSubring(name, did) => Subring::new(&name, did)?.try_into(),
            _ => Ok(Entry {
                did: self.did()?,
                data: vec![],
                kind: self.kind(),
            }),
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

    fn same_kind_as(&self, other: &Self) -> bool {
        self.kind == other.kind
    }

    fn same_key_as(&self, other: &Self) -> bool {
        self.did == other.did
    }

    // Post: result is a suffix of `data` and result.len() <= ENTRY_DATA_MAX_LEN.
    fn cap_recent_data(data: Vec<Encoded>) -> Vec<Encoded> {
        let skip_count = data.len().saturating_sub(ENTRY_DATA_MAX_LEN);
        data.into_iter().skip(skip_count).collect()
    }

    /// Normalize an entry immediately before it is persisted.
    ///
    /// Post: `result.data.len() <= ENTRY_DATA_MAX_LEN`.
    pub fn into_storage_entry(self) -> Self {
        Self {
            did: self.did,
            data: Self::cap_recent_data(self.data),
            kind: self.kind,
        }
    }

    /// The entry point of [EntryOperation].
    /// Will dispatch to different operation handlers according to the variant.
    pub fn operate(&self, op: EntryOperation) -> Result<Self> {
        match op {
            EntryOperation::Overwrite(entry) => self.overwrite(entry),
            EntryOperation::Extend(entry) => self.extend(entry),
            EntryOperation::Touch(entry) => self.touch(entry),
            EntryOperation::JoinSubring(_, did) => self.join_subring(did),
        }
    }

    /// Overwrite current data with new data.
    /// The handler of [EntryOperation::Overwrite].
    pub fn overwrite(&self, other: Self) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotOverwritable);
        }
        if !self.same_kind_as(&other) {
            return Err(Error::EntryKindNotEqual);
        }
        if !self.same_key_as(&other) {
            return Err(Error::EntryDidNotEqual);
        }
        Ok(Self {
            did: other.did,
            data: Self::cap_recent_data(other.data),
            kind: other.kind,
        })
    }

    /// This method is used to extend data to a Data kind [`Entry`].
    /// The handler of [EntryOperation::Extend].
    pub fn extend(&self, other: Self) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotAppendable);
        }
        if !self.same_kind_as(&other) {
            return Err(Error::EntryKindNotEqual);
        }
        if !self.same_key_as(&other) {
            return Err(Error::EntryDidNotEqual);
        }

        let mut data = self.data.clone();
        data.extend_from_slice(&other.data);
        let data = Self::cap_recent_data(data);

        Ok(Self {
            did: self.did,
            data,
            kind: self.kind,
        })
    }

    /// This method is used to extend data to a Data kind [`Entry`] uniquely.
    /// If any element is already existed, move it to the end of the data vector.
    /// The handler of [EntryOperation::Touch].
    pub fn touch(&self, other: Self) -> Result<Self> {
        if !self.is_data_entry() {
            return Err(Error::EntryNotAppendable);
        }
        if !self.same_kind_as(&other) {
            return Err(Error::EntryKindNotEqual);
        }
        if !self.same_key_as(&other) {
            return Err(Error::EntryDidNotEqual);
        }

        let mut data = self
            .data
            .iter()
            .filter(|e| !other.data.contains(e))
            .cloned()
            .collect::<Vec<_>>();
        data.extend_from_slice(&other.data);
        let data = Self::cap_recent_data(data);

        Ok(Self {
            did: self.did,
            data,
            kind: self.kind,
        })
    }

    /// This method is used to join a subring.
    /// The handler of [EntryOperation::JoinSubring].
    pub fn join_subring(&self, did: Did) -> Result<Self> {
        if !self.is_subring_entry() {
            return Err(Error::EntryNotJoinable);
        }

        let mut subring: Subring = self.clone().try_into()?;
        subring.finger.join(did);
        subring.try_into()
    }
}

#[cfg(test)]
mod tests {
    use num_bigint::BigUint;

    use super::*;
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
        Ok(Entry {
            did: Entry::gen_did(topic)?,
            data,
            kind: EntryKind::Data,
        })
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

    #[test]
    fn overwrite_replaces_data_for_same_data_entry() -> Result<()> {
        let entry = data_entry("topic", "old")?;
        let other = data_entry("topic", "new")?;

        let updated = entry.overwrite(other)?;

        assert_eq!(decode_entry_data(&updated)?, vec![String::from("new")]);
        Ok(())
    }

    #[test]
    fn overwrite_rejects_non_data_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let other = entry.clone();

        assert!(matches!(
            entry.overwrite(other),
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
            entry.overwrite(other),
            Err(Error::EntryKindNotEqual)
        ));
        Ok(())
    }

    #[test]
    fn overwrite_rejects_key_mismatch() -> Result<()> {
        let entry = data_entry("topic-a", "old")?;
        let other = data_entry("topic-b", "new")?;

        assert!(matches!(
            entry.overwrite(other),
            Err(Error::EntryDidNotEqual)
        ));
        Ok(())
    }

    #[test]
    fn overwrite_caps_payloads_larger_than_max_len() -> Result<()> {
        let overflow = 3;
        let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
        let entry = data_entry("topic", "base")?;

        let updated = entry.overwrite(incoming)?;

        assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
    }

    #[test]
    fn extend_appends_data_for_same_entry() -> Result<()> {
        let entry = data_entry("topic", "first")?;
        let other = data_entry("topic", "second")?;

        let updated = entry.extend(other)?;

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
            entry = entry.extend(other)?;
            assert_eq!(entry.data.len(), i + 1);
        }

        for i in ENTRY_DATA_MAX_LEN..ENTRY_DATA_MAX_LEN + 10 {
            let data = format!("test{i}");
            let other = data_entry("topic", &data)?;
            entry = entry.extend(other)?;

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

        let updated = entry.extend(incoming)?;

        assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
    }

    #[test]
    fn extend_rejects_non_data_entry() -> Result<()> {
        let entry = subring_entry("ring")?;
        let other = entry.clone();

        assert!(matches!(
            entry.extend(other),
            Err(Error::EntryNotAppendable)
        ));
        Ok(())
    }

    #[test]
    fn touch_moves_existing_items_to_end_once() -> Result<()> {
        let entry = data_entry("topic", "a")?
            .extend(data_entry("topic", "b")?)?
            .extend(data_entry("topic", "c")?)?;
        let touched = data_entry("topic", "b")?;

        let updated = entry.touch(touched)?;

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
            entry = entry.extend(data_entry("topic", &format!("test{i}"))?)?;
        }

        let updated = entry.touch(data_entry("topic", "test0")?)?;

        assert_eq!(updated.data.len(), ENTRY_DATA_MAX_LEN);
        let decoded = decode_entry_data(&updated)?;
        assert_eq!(decoded.first(), Some(&String::from("test1")));
        assert_eq!(decoded.last(), Some(&String::from("test0")));
        Ok(())
    }

    #[test]
    fn touch_caps_incoming_payloads_larger_than_max_len() -> Result<()> {
        let overflow = 3;
        let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
        let entry = data_entry("topic", "base")?;

        let updated = entry.touch(incoming)?;

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
