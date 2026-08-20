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

fn assert_entry_data_set(entry: &Entry, expected: &[&str]) -> Result<()> {
    let actual = decode_entry_data(entry)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let expected = expected
        .iter()
        .map(|value| String::from(*value))
        .collect::<BTreeSet<_>>();
    assert_eq!(actual, expected);
    Ok(())
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
    Entry::new(did, vec![encoded(value)?], EntryKind::RelayMessage).stamp_delta(version(counter))
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
    let bytes = rings_codec::serialize(&digest).map_err(Error::CodecSerialize)?;

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
fn data_compaction_prunes_tombstones_and_preserves_current_live_payloads() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let second = data_delta("topic", "second", 2)?;
    let concurrent = data_delta("topic", "concurrent", 3)?;
    let carrier = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .join(second.clone())?
        .tombstone(first.clone())?
        .join(concurrent)?;

    assert_eq!(decode_entry_data(&carrier)?, vec![
        String::from("second"),
        String::from("concurrent")
    ]);
    assert!(!carrier.crdt.tombstones.is_empty());

    let compacted = carrier.compact_data(data_entry("topic", "first")?, actor())?;

    assert_entry_data_set(&compacted, &["second", "concurrent"])?;
    assert!(compacted.crdt.register.is_some());
    assert!(compacted.crdt.tombstones.is_empty());
    assert_eq!(compacted.crdt.dots.len(), compacted.data.len());

    let joined_with_stale_add = compacted.join(first)?;
    assert_entry_data_set(&joined_with_stale_add, &["second", "concurrent"])?;
    Ok(())
}

#[test]
fn data_compaction_uses_one_shared_floor_for_divergent_replicas() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let left_live = data_delta("topic", "left-live", 2)?;
    let right_live = data_delta("topic", "right-live", 3)?;
    let tombstoned = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .tombstone(first.clone())?;
    let left = tombstoned.clone().join(left_live)?;
    let right = tombstoned.join(right_live)?;
    let op = EntryOperation::CompactData(data_entry("topic", "first")?).stamped(actor())?;

    let compacted_left = left.operate(op.clone(), Did::from(7u32))?;
    let compacted_right = right.operate(op, Did::from(8u32))?;

    assert_eq!(compacted_left.crdt.register, compacted_right.crdt.register);
    let joined = compacted_left.join(compacted_right)?;
    assert_entry_data_set(&joined, &["left-live", "right-live"])?;

    let without_left = joined.tombstone(data_entry("topic", "left-live")?)?;
    assert_eq!(decode_entry_data(&without_left)?, vec![String::from(
        "right-live"
    )]);
    Ok(())
}

#[test]
fn data_compaction_keeps_value_dots_stable_across_replica_positions() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let left_live = data_delta("topic", "left-live", 2)?;
    let shared = data_delta("topic", "shared", 3)?;
    let tombstoned = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .tombstone(first.clone())?;
    let left = tombstoned.clone().join(left_live)?.join(shared.clone())?;
    let right = tombstoned.join(shared)?;
    let op = EntryOperation::CompactData(data_entry("topic", "first")?).stamped(actor())?;

    let compacted_left = left.operate(op.clone(), Did::from(7u32))?;
    let compacted_right = right.operate(op, Did::from(8u32))?;
    let joined = compacted_left.join(compacted_right.clone())?;
    assert_entry_data_set(&joined, &["left-live", "shared"])?;

    let without_shared = joined.tombstone(data_entry("topic", "shared")?)?;
    assert_entry_data_set(&without_shared, &["left-live"])?;
    let joined_with_stale_replica = without_shared.join(compacted_right)?;
    assert_entry_data_set(&joined_with_stale_replica, &["left-live"])?;
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
    let payload = MessagePayload::new_send(Message::custom(b"relay")?, &session, signer, signer)?;
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
