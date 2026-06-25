#![warn(missing_docs)]
use std::str::FromStr;

use num_bigint::BigUint;
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
        let did = BigUint::from(msg.signer()) + BigUint::from(1u16);
        let data = msg.encode()?;
        Ok(Self {
            did: did.into(),
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
    pub fn affine(&self, scalar: u16) -> Vec<Entry> {
        self.did
            .rotate_affine(scalar)
            .iter()
            .map(|did| self.clone_with_did(did.to_owned()))
            .collect()
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

    fn trim_count(current_len: usize, incoming_len: usize) -> usize {
        current_len
            .saturating_add(incoming_len)
            .saturating_sub(ENTRY_DATA_MAX_LEN)
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
        Ok(other)
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

        let trim_num = Self::trim_count(self.data.len(), other.data.len());

        let mut data = self.data.iter().skip(trim_num).cloned().collect::<Vec<_>>();
        data.extend_from_slice(&other.data);

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

        let remains = self
            .data
            .iter()
            .filter(|e| !other.data.contains(e))
            .collect::<Vec<_>>();

        let trim_num = Self::trim_count(remains.len(), other.data.len());

        let mut data = remains
            .into_iter()
            .skip(trim_num)
            .cloned()
            .collect::<Vec<_>>();
        data.extend_from_slice(&other.data);

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

    fn decode_entry_data(entry: &Entry) -> Result<Vec<String>> {
        entry
            .data
            .iter()
            .map(|item| item.decode())
            .collect::<Result<Vec<String>>>()
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
        let affined = entry.affine(3);

        assert_eq!(affined.len(), 3);
        for rotated in affined {
            assert_eq!(rotated.data, entry.data);
            assert_eq!(rotated.kind, entry.kind);
        }
        Ok(())
    }
}
