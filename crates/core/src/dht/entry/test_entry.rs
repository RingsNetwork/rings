use super::*;
use crate::algebra::assert_join_semilattice_laws;
use crate::algebra::assert_strong_eventual_consistency;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::ENTRY_PAYLOAD_MAX_BYTES;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::tests::TEST_NETWORK_ID;

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

fn relay_entry() -> Entry {
    Entry::new(Did::from(7u32), Vec::new(), EntryKind::RelayMessage)
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

fn version_after(floor: EntryVersion, counter: u32) -> Result<EntryVersion> {
    let logical_time_ms = floor
        .logical_time_ms
        .checked_add(u128::from(counter))
        .ok_or_else(|| Error::InvalidMessage("test version overflow".to_string()))?;
    Ok(EntryVersion::new(
        logical_time_ms,
        Did::from(counter),
        Did::from(counter.saturating_add(1000)),
    ))
}

fn data_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
    data_entry(topic, value)?.stamp_delta(version(counter))
}

fn overwrite_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
    data_entry(topic, value)?.stamp_overwrite(version(counter))
}

fn compact_operation_floor(op: &EntryOperation) -> Result<EntryVersion> {
    match op {
        EntryOperation::CompactData(entry) => entry
            .crdt
            .register
            .ok_or_else(|| Error::InvalidMessage("compact op missing floor".to_string())),
        _ => Err(Error::InvalidMessage(
            "expected compact data operation".to_string(),
        )),
    }
}

fn entry_dot_for_value(entry: &Entry, value: &str) -> Result<EntryDot> {
    let encoded_value = encoded(value)?;
    entry
        .data
        .iter()
        .zip(entry.crdt.dots.iter().copied())
        .find_map(|(candidate, dot)| (candidate == &encoded_value).then_some(dot))
        .ok_or_else(|| Error::InvalidMessage(format!("missing dot for {value}")))
}

fn relay_delta(did: Did, value: &str, counter: u32) -> Result<Entry> {
    Entry::new(did, vec![encoded(value)?], EntryKind::RelayMessage).stamp_delta(version(counter))
}

#[test]
fn test_data_topic_buffer_satisfies_join_semilattice_laws() -> Result<()> {
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
fn test_relay_message_set_satisfies_join_semilattice_laws() -> Result<()> {
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
fn test_entry_join_is_strongly_eventually_consistent_for_data_deltas() -> Result<()> {
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
fn test_generic_sec_witness_accepts_data_topic_buffer_deltas() -> Result<()> {
    let base = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data).topic_buffer()?;
    let deltas = vec![
        data_delta("topic", "a", 1)?.topic_buffer()?,
        data_delta("topic", "b", 2)?.topic_buffer()?,
    ];

    assert_strong_eventual_consistency(base, &deltas);
    Ok(())
}

#[test]
fn test_storage_normalization_uses_lattice_top_n_order() -> Result<()> {
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
fn test_storage_normalization_realigns_legacy_mismatched_dots() -> Result<()> {
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
fn test_crdt_constructors_normalize_carrier_invariants() -> Result<()> {
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
fn test_overwrite_register_tiebreaker_converges_for_same_timestamp_actor() -> Result<()> {
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
fn test_operation_digest_hashes_canonical_bytes_not_legacy_base58() -> Result<()> {
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
fn test_forwarded_overwrite_witness_is_not_reissued_after_local_floor() -> Result<()> {
    let current = overwrite_delta("topic", "current", 10)?;
    let stale_forwarded = overwrite_delta("topic", "stale", 1)?;

    let updated = current.overwrite(NOW_MS, stale_forwarded, actor())?;

    assert_eq!(decode_entry_data(&updated)?, vec![String::from("current")]);
    Ok(())
}

#[test]
fn test_overwrite_replaces_data_for_same_data_entry() -> Result<()> {
    let entry = data_entry("topic", "old")?;
    let other = data_entry("topic", "new")?;
    let updated = entry.overwrite(NOW_MS, other, actor())?;
    assert_eq!(decode_entry_data(&updated)?, vec![String::from("new")]);
    Ok(())
}

#[test]
fn test_overwrite_rejects_non_data_entry() -> Result<()> {
    let entry = relay_entry();
    let other = entry.clone();

    assert!(matches!(
        entry.overwrite(NOW_MS, other, actor()),
        Err(Error::EntryNotOverwritable)
    ));
    Ok(())
}

#[test]
fn test_overwrite_rejects_kind_mismatch() -> Result<()> {
    let entry = data_entry("topic", "old")?;
    let mut other = entry.clone();
    other.kind = EntryKind::RelayMessage;

    assert!(matches!(
        entry.overwrite(NOW_MS, other, actor()),
        Err(Error::EntryKindNotEqual)
    ));
    Ok(())
}

#[test]
fn test_overwrite_rejects_key_mismatch() -> Result<()> {
    let entry = data_entry("topic-a", "old")?;
    let other = data_entry("topic-b", "new")?;

    assert!(matches!(
        entry.overwrite(NOW_MS, other, actor()),
        Err(Error::EntryDidNotEqual)
    ));
    Ok(())
}

#[test]
fn test_overwrite_caps_payloads_larger_than_max_len() -> Result<()> {
    let overflow = 3;
    let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
    let entry = data_entry("topic", "base")?;
    let updated = entry.overwrite(NOW_MS, incoming, actor())?;
    assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
}

#[test]
fn test_extend_appends_data_for_same_entry() -> Result<()> {
    let entry = data_entry("topic", "first")?;
    let other = data_entry("topic", "second")?;
    let updated = entry.extend(NOW_MS, other, actor())?;
    assert_eq!(decode_entry_data(&updated)?, vec![
        String::from("first"),
        String::from("second")
    ]);
    Ok(())
}

#[test]
fn test_extend_trims_oldest_items_at_max_len() -> Result<()> {
    let mut entry = data_entry("topic", "test0")?;
    for i in 1..ENTRY_DATA_MAX_LEN {
        let data = format!("test{i}");
        let other = data_entry("topic", &data)?;
        entry = entry.extend(NOW_MS, other, actor())?;
        assert_eq!(entry.data.len(), i + 1);
    }

    for i in ENTRY_DATA_MAX_LEN..ENTRY_DATA_MAX_LEN + 10 {
        let data = format!("test{i}");
        let other = data_entry("topic", &data)?;
        entry = entry.extend(NOW_MS, other, actor())?;
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
fn test_extend_caps_incoming_payloads_larger_than_max_len() -> Result<()> {
    let overflow = 3;
    let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
    let entry = data_entry("topic", "base")?;
    let updated = entry.extend(NOW_MS, incoming, actor())?;
    assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
}

/// Extend is the element-set join for both carriers, so a relay inbox grows by extension;
/// touch remains a data-topic operation.
#[test]
fn test_extend_grows_relay_inbox_but_touch_rejects_it() -> Result<()> {
    let inbox = relay_entry();
    let delta = relay_delta(inbox.did, "m1", 1)?;

    let extended = inbox.extend(NOW_MS, delta.clone(), actor())?;
    assert_eq!(extended.data, delta.data);
    assert!(matches!(
        inbox.touch(NOW_MS, delta, actor()),
        Err(Error::EntryNotAppendable)
    ));
    Ok(())
}

#[test]
fn test_touch_moves_existing_items_to_end_once() -> Result<()> {
    let entry = data_entry("topic", "a")?
        .extend(NOW_MS, data_entry("topic", "b")?, actor())?
        .extend(NOW_MS, data_entry("topic", "c")?, actor())?;
    let touched = data_entry("topic", "b")?;
    let updated = entry.touch(NOW_MS, touched, actor())?;
    assert_eq!(decode_entry_data(&updated)?, vec![
        String::from("a"),
        String::from("c"),
        String::from("b")
    ]);
    Ok(())
}

#[test]
fn test_touch_trims_oldest_non_touched_items_at_max_len() -> Result<()> {
    let mut entry = data_entry("topic", "test0")?;
    for i in 1..ENTRY_DATA_MAX_LEN {
        entry = entry.extend(NOW_MS, data_entry("topic", &format!("test{i}"))?, actor())?;
    }
    let updated = entry.touch(NOW_MS, data_entry("topic", "test0")?, actor())?;
    assert_eq!(updated.data.len(), ENTRY_DATA_MAX_LEN);
    let decoded = decode_entry_data(&updated)?;
    assert_eq!(decoded.first(), Some(&String::from("test1")));
    assert_eq!(decoded.last(), Some(&String::from("test0")));
    Ok(())
}

#[test]
fn test_relay_tombstone_removes_observed_message_by_join() -> Result<()> {
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
fn test_data_tombstone_removes_observed_payload_by_join() -> Result<()> {
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
fn test_data_compaction_prunes_tombstones_and_preserves_current_live_payloads() -> Result<()> {
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

    let compacted = carrier.compact_data(NOW_MS, data_entry("topic", "first")?, actor())?;

    assert_entry_data_set(&compacted, &["second", "concurrent"])?;
    assert!(compacted.crdt.register.is_some());
    assert!(compacted.crdt.tombstones.is_empty());
    assert_eq!(compacted.crdt.dots.len(), compacted.data.len());

    let joined_with_stale_add = compacted.join(first)?;
    assert_entry_data_set(&joined_with_stale_add, &["second", "concurrent"])?;
    Ok(())
}

#[test]
fn test_data_compaction_uses_one_shared_floor_for_divergent_replicas() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let left_live = data_delta("topic", "left-live", 2)?;
    let right_live = data_delta("topic", "right-live", 3)?;
    let tombstoned = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .tombstone(first.clone())?;
    let left = tombstoned.clone().join(left_live)?;
    let right = tombstoned.join(right_live)?;
    let op = EntryOperation::CompactData(data_entry("topic", "first")?).stamped(NOW_MS, actor())?;

    let compacted_left = left.operate(NOW_MS, op.clone(), Did::from(7u32))?;
    let compacted_right = right.operate(NOW_MS, op, Did::from(8u32))?;

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
fn test_data_compaction_keeps_value_dots_stable_across_replica_positions() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let left_live = data_delta("topic", "left-live", 2)?;
    let shared = data_delta("topic", "shared", 3)?;
    let tombstoned = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .tombstone(first.clone())?;
    let left = tombstoned.clone().join(left_live)?.join(shared.clone())?;
    let right = tombstoned.join(shared)?;
    let op = EntryOperation::CompactData(data_entry("topic", "first")?).stamped(NOW_MS, actor())?;

    let compacted_left = left.operate(NOW_MS, op.clone(), Did::from(7u32))?;
    let compacted_right = right.operate(NOW_MS, op, Did::from(8u32))?;
    let joined = compacted_left.join(compacted_right.clone())?;
    assert_entry_data_set(&joined, &["left-live", "shared"])?;

    let without_shared = joined.tombstone(data_entry("topic", "shared")?)?;
    assert_entry_data_set(&without_shared, &["left-live"])?;
    let joined_with_stale_replica = without_shared.join(compacted_right)?;
    assert_entry_data_set(&joined_with_stale_replica, &["left-live"])?;
    Ok(())
}

#[test]
fn test_delayed_data_compaction_preserves_post_floor_writes() -> Result<()> {
    let first = data_delta("topic", "first", 1)?;
    let second = data_delta("topic", "second", 2)?;
    let tombstoned = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(first.clone())?
        .join(second)?
        .tombstone(first.clone())?;
    let op = EntryOperation::CompactData(data_entry("topic", "first")?).stamped(NOW_MS, actor())?;
    let floor = compact_operation_floor(&op)?;
    let readded_first = data_entry("topic", "first")?.stamp_delta(version_after(floor, 101)?)?;
    let late_live = data_entry("topic", "late-live")?.stamp_delta(version_after(floor, 102)?)?;
    let readded_first_dot = entry_dot_for_value(&readded_first, "first")?;
    let late_live_dot = entry_dot_for_value(&late_live, "late-live")?;
    let with_post_floor_writes = tombstoned.join(readded_first)?.join(late_live)?;

    let compacted = with_post_floor_writes.operate(NOW_MS, op, Did::from(7u32))?;

    assert_entry_data_set(&compacted, &["first", "second", "late-live"])?;
    assert_eq!(entry_dot_for_value(&compacted, "first")?, readded_first_dot);
    assert_eq!(entry_dot_for_value(&compacted, "late-live")?, late_live_dot);
    let joined_with_stale_add = compacted.join(first)?;
    assert_entry_data_set(&joined_with_stale_add, &["first", "second", "late-live"])?;
    Ok(())
}

#[test]
fn test_delayed_data_compaction_preserves_newer_register_floor() -> Result<()> {
    let op =
        EntryOperation::CompactData(data_entry("topic", "obsolete")?).stamped(NOW_MS, actor())?;
    let compact_floor = compact_operation_floor(&op)?;
    let stale_after_compact =
        data_entry("topic", "stale")?.stamp_delta(version_after(compact_floor, 1)?)?;
    let reset =
        data_entry("topic", "reset")?.stamp_overwrite(version_after(compact_floor, 100)?)?;
    let reset_floor = reset
        .crdt
        .register
        .ok_or_else(|| Error::InvalidMessage("reset missing register".to_string()))?;
    let state = Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data)
        .join(stale_after_compact.clone())?
        .join(reset)?;

    assert_entry_data_set(&state, &["reset"])?;
    assert_eq!(state.crdt.register, Some(reset_floor));

    let compacted = state.operate(NOW_MS, op, Did::from(7u32))?;

    assert_entry_data_set(&compacted, &["reset"])?;
    assert_eq!(compacted.crdt.register, Some(reset_floor));
    let joined_with_stale_add = compacted.join(stale_after_compact)?;
    assert_entry_data_set(&joined_with_stale_add, &["reset"])?;
    Ok(())
}

#[test]
fn test_touch_caps_incoming_payloads_larger_than_max_len() -> Result<()> {
    let overflow = 3;
    let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
    let entry = data_entry("topic", "base")?;
    let updated = entry.touch(NOW_MS, incoming, actor())?;
    assert_entry_keeps_recent_overflow(&updated, incoming_count, overflow)
}

#[test]
fn test_operation_default_entry_matches_operation_kind() -> Result<()> {
    let target = data_entry("topic", "value")?;
    let default = EntryOperation::Extend(target.clone()).gen_default_entry()?;
    assert_eq!(default.did, target.did);
    assert_eq!(default.kind, EntryKind::Data);
    assert!(default.data.is_empty());
    Ok(())
}

#[test]
fn test_affine_preserves_payload_and_kind_while_rotating_keys() -> Result<()> {
    let entry = data_entry("topic", "value")?;
    let affined = entry.affine(3)?;
    assert_eq!(affined.len(), 3);
    for rotated in affined {
        assert_eq!(rotated.data, entry.data);
        assert_eq!(rotated.kind, entry.kind);
    }
    Ok(())
}

const NOW_MS: u128 = 1_700_000_000_000;

fn bounded(mut entry: Entry, expires_at_ms: u128) -> Entry {
    entry.expires_at_ms = Some(expires_at_ms);
    entry
}

fn admissible_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
    Ok(bounded(data_delta(topic, value, counter)?, NOW_MS + 1_000))
}

fn version_at(logical_time_ms: u128) -> EntryVersion {
    EntryVersion::new(logical_time_ms, actor(), Did::from(1u32))
}

/// Law: the operation boundary stamps `now + DEFAULT_TTL_MS` on every variant that carries no
/// bound and preserves a bound the origin already stamped.
#[test]
fn test_stamped_assigns_default_lifetime_and_preserves_existing() -> Result<()> {
    let expected = NOW_MS + u128::from(DEFAULT_TTL_MS);
    let unstamped = data_entry("topic", "value")?;
    let ops = [
        EntryOperation::Overwrite(unstamped.clone()),
        EntryOperation::Extend(unstamped.clone()),
        EntryOperation::Touch(unstamped.clone()),
        EntryOperation::Tombstone(unstamped.clone()),
        EntryOperation::CompactData(unstamped.clone()),
    ];
    for op in ops {
        let stamped = op.stamped(NOW_MS, actor())?;
        assert_eq!(stamped.entry().expires_at_ms, Some(expected));
    }

    let forwarded = EntryOperation::Extend(bounded(unstamped, 7)).stamped(NOW_MS, actor())?;
    assert_eq!(forwarded.entry().expires_at_ms, Some(7));
    Ok(())
}

/// Law: the retention bound joins by `max`, commutatively, for data and relay carriers and for
/// every operation that materializes a join.
#[test]
fn test_join_takes_the_later_retention_bound() -> Result<()> {
    let earlier = bounded(data_delta("topic", "a", 1)?, 10);
    let later = bounded(data_delta("topic", "b", 2)?, 20);
    assert_eq!(earlier.join(later.clone())?.expires_at_ms, Some(20));
    assert_eq!(later.join(earlier.clone())?.expires_at_ms, Some(20));

    let relay_earlier = bounded(relay_delta(Did::from(7u32), "m1", 1)?, 10);
    let relay_later = bounded(relay_delta(Did::from(7u32), "m2", 2)?, 20);
    assert_eq!(relay_earlier.join(relay_later)?.expires_at_ms, Some(20));

    let tombstoned = earlier.tombstone(bounded(data_entry("topic", "a")?, 30))?;
    assert_eq!(tombstoned.expires_at_ms, Some(30));

    let compacted =
        earlier.compact_data(NOW_MS, bounded(data_entry("topic", "a")?, 40), actor())?;
    assert_eq!(compacted.expires_at_ms, Some(40));

    let unbounded_join = data_delta("topic", "c", 3)?.join(earlier.clone())?;
    assert_eq!(unbounded_join.expires_at_ms, Some(10));
    Ok(())
}

/// Admission law: every payload is at most `ENTRY_PAYLOAD_MAX_BYTES` encoded bytes. The bound is
/// per element, so the carrier stays a lattice and its size is bounded by the count cap.
#[test]
fn test_admission_bounds_every_payload_size() -> Result<()> {
    let did = Entry::gen_did("topic")?;
    let at_bound = Entry::new(
        did,
        vec![Encoded::from("x".repeat(ENTRY_PAYLOAD_MAX_BYTES))],
        EntryKind::Data,
    );
    bounded(at_bound, NOW_MS + 1_000).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;

    let oversize = Entry::new(
        did,
        vec![
            Encoded::from("small"),
            Encoded::from("x".repeat(ENTRY_PAYLOAD_MAX_BYTES + 1)),
        ],
        EntryKind::Data,
    );
    assert!(matches!(
        bounded(oversize, NOW_MS + 1_000).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryPayloadExceedsMax)
    ));
    Ok(())
}

/// Law: an entry is live exactly when it carries a bound strictly after `now`.
#[test]
fn test_is_live_at_requires_a_bound_after_now() -> Result<()> {
    let unstamped = data_entry("topic", "value")?;
    assert!(!unstamped.is_live_at(0));
    let stamped = bounded(unstamped, 5);
    assert!(stamped.is_live_at(4));
    assert!(!stamped.is_live_at(5));
    Ok(())
}

/// Admission law: the bound must be live and at most `now + MAX_TTL_MS + TS_OFFSET_TOLERANCE_MS`.
#[test]
fn test_admission_bounds_the_retention_bound() -> Result<()> {
    let limit = NOW_MS + u128::from(MAX_TTL_MS) + TS_OFFSET_TOLERANCE_MS;
    let delta = data_delta("topic", "value", 1)?;

    assert!(matches!(
        delta.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryNotLive)
    ));
    assert!(matches!(
        bounded(delta.clone(), NOW_MS).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryNotLive)
    ));
    assert!(matches!(
        bounded(delta.clone(), limit + 1).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryLifetimeExceedsMax)
    ));
    bounded(delta.clone(), limit).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
    bounded(delta, NOW_MS + 1).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
    Ok(())
}

/// Admission law: every carried version (dots, tombstones, register) has a logical time at most
/// `now + TS_OFFSET_TOLERANCE_MS`, so a peer-supplied `u128::MAX` floor cannot pin a key.
#[test]
fn test_admission_bounds_every_version_logical_time() -> Result<()> {
    let clock_bound = NOW_MS + TS_OFFSET_TOLERANCE_MS;
    let base = admissible_delta("topic", "value", 1)?;

    let mut ahead_dot = base.clone();
    ahead_dot.crdt.dots = vec![EntryDot::for_index(version_at(clock_bound + 1), 0)?];
    assert!(matches!(
        ahead_dot.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryVersionAheadOfClock)
    ));

    let mut ahead_register = base.clone();
    ahead_register.crdt.register = Some(version_at(u128::MAX));
    assert!(matches!(
        ahead_register.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryVersionAheadOfClock)
    ));

    let mut ahead_tombstone = base.clone();
    ahead_tombstone.crdt.tombstones = vec![EntryDot::for_index(version_at(clock_bound + 1), 0)?];
    assert!(matches!(
        ahead_tombstone.validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryVersionAheadOfClock)
    ));

    let mut at_bound = base;
    at_bound.crdt.dots = vec![EntryDot::for_index(version_at(clock_bound), 0)?];
    at_bound.crdt.register = Some(version_at(clock_bound));
    at_bound.validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
    Ok(())
}

/// Storage normalization and affine placement preserve the retention bound.
#[test]
fn test_normalization_and_affine_preserve_retention_bound() -> Result<()> {
    let entry = admissible_delta("topic", "value", 1)?;
    assert_eq!(
        entry.clone().try_into_storage_entry()?.expires_at_ms,
        entry.expires_at_ms
    );
    for replica in entry.affine(3)? {
        assert_eq!(replica.expires_at_ms, entry.expires_at_ms);
    }
    Ok(())
}

/// A stored value written before retention bounds existed deserializes as unstamped and is
/// therefore not live, so it is retired on its next read instead of being served forever.
#[test]
fn test_legacy_value_without_bound_is_not_live() -> Result<()> {
    let mut legacy = serde_json::to_value(admissible_delta("topic", "value", 1)?)
        .map_err(|_| Error::SerializeToString)?;
    legacy
        .as_object_mut()
        .ok_or_else(|| Error::InvalidMessage("entry must serialize to an object".to_string()))?
        .remove("expires_at_ms");
    let entry: Entry = serde_json::from_value(legacy).map_err(Error::Deserialize)?;
    assert_eq!(entry.expires_at_ms, None);
    assert!(!entry.is_live_at(0));
    Ok(())
}
