use bytes::Bytes;

use super::*;
use crate::algebra::assert_join_semilattice_laws;
use crate::algebra::assert_strong_eventual_consistency;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::ENTRY_PAYLOAD_MAX_BYTES;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::message::Encoder;
use crate::tests::with_retention;
use crate::tests::TEST_NETWORK_ID;

const NOW_MS: u128 = 1_700_000_000_000;

/// The data-topic element holding the UTF-8 bytes of `value`.
fn element(value: &str) -> Result<Bytes> {
    Ok(Bytes::from(value.to_string()))
}

fn data_entry(topic: &str, value: &str) -> Result<Entry> {
    (topic.to_string(), element(value)?).try_into()
}

fn data_entry_from_values(topic: &str, values: Vec<String>) -> Result<Entry> {
    let data = values.into_iter().map(Bytes::from).collect::<Vec<_>>();
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
        .map(|item| String::from_utf8(item.to_vec()).map_err(|_| Error::Decode))
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

fn entry_dot_for_value(entry: &Entry, value: &str) -> Result<EntryDot> {
    let encoded_value = element(value)?;
    entry
        .data
        .iter()
        .zip(entry.crdt.dots.iter().copied())
        .find_map(|(candidate, dot)| (candidate == &encoded_value).then_some(dot))
        .ok_or_else(|| Error::InvalidMessage(format!("missing dot for {value}")))
}

fn relay_delta(did: Did, value: &str, counter: u32) -> Result<Entry> {
    Entry::new(did, vec![element(value)?], EntryKind::RelayMessage).stamp_delta(version(counter))
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
    let a = Entry::new(did, vec![element("a")?], EntryKind::RelayMessage)
        .stamp_delta(version(1))?
        .topic_buffer()?;
    let b = Entry::new(did, vec![element("b")?], EntryKind::RelayMessage)
        .stamp_delta(version(2))?
        .topic_buffer()?;
    let ab = Entry::new(did, vec![], EntryKind::RelayMessage)
        .join(relay_delta(did, "a", 1)?)?
        .join(relay_delta(did, "b", 2)?)?;
    let tombstoned_a = ab.tombstone(relay_delta(did, "a", 1)?)?.topic_buffer()?;

    assert_join_semilattice_laws(&[DataTopicBuffer::default(), a, b, tombstoned_a]);
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
    let stale = element("stale")?;
    let live = element("live")?;
    let mut values = BTreeMap::new();
    values.insert(stale.clone(), EntryDot::for_index(version(1), 0)?);
    let live_dot = EntryDot::for_index(version(11), 0)?;
    values.insert(live.clone(), live_dot);

    let buffer = DataTopicBuffer::new(Some(register), values, BTreeMap::new());
    assert_eq!(buffer.values.len(), 1);
    assert!(buffer.values.contains_key(&live));

    let removes = BTreeMap::from([(ElementDigest::of(&live), live_dot)]);
    let tombstoned = DataTopicBuffer::new(buffer.register, buffer.values, removes);
    assert!(tombstoned.values.is_empty());
    assert_eq!(
        tombstoned.removes.get(&ElementDigest::of(&live)),
        Some(&live_dot)
    );
    Ok(())
}

#[test]
fn test_overwrite_register_tiebreaker_converges_for_same_timestamp_actor() -> Result<()> {
    let did = Entry::gen_did("topic")?;
    let issuer = actor();
    let lower = Entry::new(did, vec![element("lower")?], EntryKind::Data)
        .stamp_overwrite(EntryVersion::new(1, issuer, Did::from(1u32)))?;
    let higher = Entry::new(did, vec![element("higher")?], EntryKind::Data)
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

/// Extend is the element-set join for both carriers, so a relay inbox grows by extension.
#[test]
fn test_extend_grows_relay_inbox() -> Result<()> {
    let inbox = relay_entry();
    let delta = relay_delta(inbox.did, "m1", 1)?;

    let extended = inbox.extend(NOW_MS, delta.clone(), actor())?;
    assert_eq!(extended.data, delta.data);
    Ok(())
}

/// Re-extending an existing payload moves it to the end: the element set keeps
/// the maximal dot per value and materialises in dot order.
#[test]
fn test_extend_moves_existing_items_to_end_once() -> Result<()> {
    let entry = data_entry("topic", "a")?
        .extend(NOW_MS, data_entry("topic", "b")?, actor())?
        .extend(NOW_MS, data_entry("topic", "c")?, actor())?;
    let touched = data_entry("topic", "b")?;
    let updated = entry.extend(NOW_MS, touched, actor())?;
    assert_eq!(decode_entry_data(&updated)?, vec![
        String::from("a"),
        String::from("c"),
        String::from("b")
    ]);
    Ok(())
}

#[test]
fn test_extend_of_existing_item_at_max_len_moves_it_to_end() -> Result<()> {
    let mut entry = data_entry("topic", "test0")?;
    for i in 1..ENTRY_DATA_MAX_LEN {
        entry = entry.extend(NOW_MS, data_entry("topic", &format!("test{i}"))?, actor())?;
    }
    let updated = entry.extend(NOW_MS, data_entry("topic", "test0")?, actor())?;
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
fn test_extend_caps_incoming_payloads_larger_than_max_len_over_base() -> Result<()> {
    let overflow = 3;
    let (incoming, incoming_count) = overflowing_data_entry("topic", overflow)?;
    let entry = data_entry("topic", "base")?;
    let updated = entry.extend(NOW_MS, incoming, actor())?;
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

/// Replica placement rotates the storage key while preserving the entire
/// resource carrier, including its DID, kind, payload, and CRDT metadata.
#[test]
fn test_affine_placement_preserves_the_resource_carrier() -> Result<()> {
    // The data carrier is shared by replicas at three distinct placement keys.
    let entry = data_entry("topic", "value")?;
    let placements = entry.did.rotate_affine(3)?;
    assert_eq!(placements.len(), 3);
    assert!(placements.iter().any(|key| *key != entry.did));
    for key in placements {
        // A replica changes its location rather than the resource identity.
        let replica = PlacedEntry::new(key, entry.clone());
        replica.validate_placement(3)?;
        assert_eq!(replica.key, key);
        assert_eq!(replica.entry, entry);
    }
    Ok(())
}

fn admissible_delta(topic: &str, value: &str, counter: u32) -> Result<Entry> {
    Ok(with_retention(
        data_delta(topic, value, counter)?,
        NOW_MS + 1_000,
    ))
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
        EntryOperation::Tombstone(unstamped.clone()),
    ];
    for op in ops {
        let stamped = op.stamped(NOW_MS, actor())?;
        assert_eq!(stamped.entry().expires_at_ms, Some(expected));
    }

    let forwarded =
        EntryOperation::Extend(with_retention(unstamped, 7)).stamped(NOW_MS, actor())?;
    assert_eq!(forwarded.entry().expires_at_ms, Some(7));
    Ok(())
}

/// Law: the retention bound joins by `max`, commutatively, for data and relay carriers and for
/// every operation that materializes a join.
#[test]
fn test_join_takes_the_later_retention_bound() -> Result<()> {
    let earlier = with_retention(data_delta("topic", "a", 1)?, 10);
    let later = with_retention(data_delta("topic", "b", 2)?, 20);
    assert_eq!(earlier.join(later.clone())?.expires_at_ms, Some(20));
    assert_eq!(later.join(earlier.clone())?.expires_at_ms, Some(20));

    let relay_earlier = with_retention(relay_delta(Did::from(7u32), "m1", 1)?, 10);
    let relay_later = with_retention(relay_delta(Did::from(7u32), "m2", 2)?, 20);
    assert_eq!(relay_earlier.join(relay_later)?.expires_at_ms, Some(20));

    let overwritten = earlier.overwrite(
        NOW_MS,
        with_retention(data_entry("topic", "a")?, 40),
        actor(),
    )?;
    assert_eq!(overwritten.expires_at_ms, Some(40));

    let unbounded_join = data_delta("topic", "c", 3)?.join(earlier.clone())?;
    assert_eq!(unbounded_join.expires_at_ms, Some(10));
    Ok(())
}

/// Removal law: a tombstone leaves the carrier's retention bound unchanged, for a data topic
/// and a relay inbox alike, however far ahead the removal's own bound lies.
#[test]
fn test_removal_leaves_the_retention_bound_unchanged() -> Result<()> {
    let topic = with_retention(data_delta("topic", "a", 1)?, 10);
    let removal = with_retention(data_entry("topic", "a")?, 30);
    let drained = topic.tombstone(removal)?;
    assert!(drained.data.is_empty());
    assert_eq!(drained.expires_at_ms, Some(10));

    let inbox = with_retention(relay_delta(Did::from(7u32), "m1", 1)?, 10);
    let removal = with_retention(inbox.removal_of(inbox.crdt.dots.clone()), 30);
    let drained = inbox.tombstone(removal)?;
    assert!(drained.data.is_empty());
    assert_eq!(drained.expires_at_ms, Some(10));
    Ok(())
}

/// Admission law: every payload is at most `ENTRY_PAYLOAD_MAX_BYTES` bytes. The bound is
/// per element, so the carrier stays a lattice and its size is bounded by the count cap.
#[test]
fn test_admission_bounds_every_payload_size() -> Result<()> {
    let did = Entry::gen_did("topic")?;
    let at_bound = Entry::new(
        did,
        vec![Bytes::from("x".repeat(ENTRY_PAYLOAD_MAX_BYTES))],
        EntryKind::Data,
    );
    with_retention(at_bound, NOW_MS + 1_000).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;

    let oversize = Entry::new(
        did,
        vec![
            Bytes::from("small"),
            Bytes::from("x".repeat(ENTRY_PAYLOAD_MAX_BYTES + 1)),
        ],
        EntryKind::Data,
    );
    assert!(matches!(
        with_retention(oversize, NOW_MS + 1_000).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryPayloadExceedsMax)
    ));
    Ok(())
}

/// Law: an entry is live exactly when it carries a bound strictly after `now`.
#[test]
fn test_is_live_at_requires_a_bound_after_now() -> Result<()> {
    let unstamped = data_entry("topic", "value")?;
    assert!(!unstamped.is_live_at(0));
    let stamped = with_retention(unstamped, 5);
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
        with_retention(delta.clone(), NOW_MS).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryNotLive)
    ));
    assert!(matches!(
        with_retention(delta.clone(), limit + 1).validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
        Err(Error::EntryLifetimeExceedsMax)
    ));
    with_retention(delta.clone(), limit).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
    with_retention(delta, NOW_MS + 1).validate_admissible_at(NOW_MS, TEST_NETWORK_ID)?;
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
    ahead_tombstone.crdt.tombstones = vec![EntryTombstone::of(
        &element("value")?,
        EntryDot::for_index(version_at(clock_bound + 1), 0)?,
    )];
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
    // Retention belongs to the carrier and survives both normalization and placement.
    let entry = admissible_delta("topic", "value", 1)?;
    assert_eq!(
        entry.clone().try_into_storage_entry()?.expires_at_ms,
        entry.expires_at_ms
    );
    for key in entry.did.rotate_affine(3)? {
        // Every physical replica retains the original carrier's expiration.
        let replica = PlacedEntry::new(key, entry.clone());
        assert_eq!(replica.entry.expires_at_ms, entry.expires_at_ms);
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

/// The data element horizon `H`.
fn data_horizon_ms() -> u128 {
    u128::from(MAX_TTL_MS)
}

/// A data delta for `value` whose dot is issued `offset_ms` after [`NOW_MS`], by a writer
/// named after `offset_ms`.
fn delta_at(value: &str, offset_ms: u32) -> Result<Entry> {
    data_entry("topic", value)?.stamp_delta(version_after(version_at(NOW_MS), offset_ms)?)
}

/// A data delta for `value` issued at `logical_time_ms` by `writer`.
fn delta_by(value: &str, logical_time_ms: u128, writer: u32) -> Result<Entry> {
    data_entry("topic", value)?.stamp_delta(EntryVersion::new(
        logical_time_ms,
        Did::from(writer),
        Did::from(writer.saturating_add(1000)),
    ))
}

/// The empty data carrier of the test topic, with a retention bound two horizons ahead.
fn empty_topic() -> Result<Entry> {
    Ok(with_retention(
        Entry::new(Entry::gen_did("topic")?, vec![], EntryKind::Data),
        NOW_MS + 2 * data_horizon_ms(),
    ))
}

/// #867 regression: a concurrent add one owner never received survives every join with that
/// owner, however that owner replaces its own values, and a reader that saw the add through
/// the other replica keeps it (the `NotEnoughLoopHops { 4, 3 }` trace of #864). No operation
/// but a user `Overwrite` issues a reset floor any more.
#[test]
fn test_concurrent_add_unseen_by_one_owner_survives() -> Result<()> {
    // Replica Y holds r4, issued before everything owner X does; X never receives it.
    let r4 = delta_at("r4", 1)?;
    let replica_y = empty_topic()?.join(r4.clone())?;
    // X replaces its own descriptor: an add, then a removal of the value it replaces.
    let owner_x = empty_topic()?
        .join(delta_at("old", 2)?)?
        .join(delta_at("new", 3)?)?
        .tombstone(data_entry("topic", "old")?)?;
    assert_eq!(owner_x.crdt.register, None);
    let removal =
        EntryOperation::Tombstone(data_entry("topic", "old")?).stamped(NOW_MS, actor())?;
    assert_eq!(removal.entry().crdt.register, None);

    let later = NOW_MS + 1_000;
    for joined in [
        owner_x.join(replica_y.clone())?,
        replica_y.join(owner_x.clone())?,
    ] {
        let joined = joined.retired_at(later);
        assert_entry_data_set(&joined, &["r4", "new"])?;
        assert_eq!(
            entry_dot_for_value(&joined, "r4")?,
            entry_dot_for_value(&r4, "r4")?
        );
    }
    let reader = replica_y
        .retired_at(later)
        .join(owner_x.retired_at(later))?;
    assert_entry_data_set(&reader, &["r4", "new"])
}

/// #874 regression, the model trace of the issue: owner 0 forgets `a@d0` under `a@d1`, removes
/// `a`, and a reader cache still holding `a@d0` does not resurrect it in any join; an add of
/// `a` above the remove wins over it.
#[test]
fn test_remove_covers_superseded_dot_of_same_payload() -> Result<()> {
    let d0 = delta_by("a", NOW_MS, 0)?;
    let d1 = delta_by("a", NOW_MS + 1, 1)?;
    let owner0 = empty_topic()?.join(d0.clone())?;
    // Sync(0 → cache): the cache holds a@d0.
    let cache = owner0.clone();
    let owner1 = empty_topic()?.join(d1.clone())?;
    // Sync(1 → 0): owner 0 keeps a ↦ d1 and forgets d0.
    let owner0 = owner0.join(owner1.clone())?;
    assert_eq!(
        entry_dot_for_value(&owner0, "a")?,
        entry_dot_for_value(&d1, "a")?
    );
    // Remove(0, a).
    let owner0 = owner0.tombstone(data_entry("topic", "a")?)?;

    let exchanged = owner0.join(owner1)?.join(cache.clone())?;
    assert!(exchanged.data.is_empty());
    assert!(cache.join(owner0.clone())?.data.is_empty());
    assert!(owner0.join(d0)?.data.is_empty());
    assert_eq!(owner0.crdt.tombstones.len(), 1);

    let d2 = delta_by("a", NOW_MS + 2, 2)?;
    assert_entry_data_set(&owner0.join(d2)?, &["a"])
}

/// Horizon law for adds: a data element is visible strictly before `τ(d) + H` and retired from
/// `τ(d) + H` on, even while a later write keeps its carrier alive.
#[test]
fn test_data_element_retires_at_its_horizon() -> Result<()> {
    let horizon = data_horizon_ms();
    let carrier = with_retention(delta_at("a", 0)?, NOW_MS + 2 * horizon);
    assert_entry_data_set(&carrier.clone().retired_at(NOW_MS + horizon - 1), &["a"])?;
    assert!(carrier.clone().retired_at(NOW_MS + horizon).data.is_empty());

    let refreshed = carrier.join(delta_at("b", 1_000)?)?;
    assert_entry_data_set(&refreshed.clone().retired_at(NOW_MS + horizon), &["b"])?;
    // Writing `a` again issues a fresh dot, which is what keeps it.
    let rewritten = refreshed.join(delta_at("a", 2_000)?)?;
    assert_entry_data_set(&rewritten.retired_at(NOW_MS + horizon), &["a", "b"])
}

/// Horizon law for removes: a remove is held strictly before `τ(r) + H + σ`, so it outlives
/// every add it covers on every clock within the skew tolerance, and is collected from then on.
#[test]
fn test_data_remove_retires_after_horizon_and_skew() -> Result<()> {
    let horizon = data_horizon_ms();
    let removed = with_retention(delta_at("a", 0)?, NOW_MS + 2 * horizon)
        .tombstone(data_entry("topic", "a")?)?;
    let stable_at = NOW_MS + horizon + TS_OFFSET_TOLERANCE_MS;
    let held = removed.clone().retired_at(stable_at - 1);
    assert_eq!(held.crdt.tombstones.len(), 1);
    // A stale replica read on the latest clock the remove is still held on cannot resurrect.
    assert!(held.join(delta_at("a", 0)?)?.data.is_empty());
    assert!(removed.retired_at(stable_at).crdt.tombstones.is_empty());
    Ok(())
}

/// Homomorphism and composition laws of `retire_t` on a sample of carriers with adds,
/// superseded dots, removes, and an overwrite floor, at clocks on either side of both
/// thresholds.
#[test]
fn test_retire_is_a_join_homomorphism() -> Result<()> {
    let horizon = data_horizon_ms();
    let sigma = TS_OFFSET_TOLERANCE_MS;
    let with_bound = |entry: Entry| with_retention(entry, NOW_MS + 3 * horizon);
    let early_a = with_bound(delta_by("a", NOW_MS, 0)?);
    let late_a = with_bound(delta_by("a", NOW_MS + sigma, 1)?);
    let b = with_bound(delta_by("b", NOW_MS + horizon / 2, 2)?);
    let removed_a = early_a
        .join(late_a.clone())?
        .tombstone(data_entry("topic", "a")?)?;
    let overwritten = with_bound(
        data_entry("topic", "c")?.stamp_overwrite(version_after(version_at(NOW_MS), 3_000)?)?,
    );
    let samples = [
        empty_topic()?,
        early_a,
        late_a,
        b.clone(),
        removed_a.clone(),
        removed_a.join(b)?,
        overwritten,
    ];
    let clocks = [
        NOW_MS,
        NOW_MS + horizon - 1,
        NOW_MS + horizon,
        NOW_MS + horizon + sigma - 1,
        NOW_MS + horizon + sigma,
        NOW_MS + horizon + 2 * sigma,
        NOW_MS + horizon * 3 / 2 + sigma,
    ];
    for x in samples.iter() {
        for y in samples.iter() {
            for t in clocks {
                let lhs = x.join(y.clone())?.retired_at(t);
                let rhs = x
                    .clone()
                    .retired_at(t)
                    .join(y.clone().retired_at(t))?
                    .try_into_storage_entry()?;
                assert_eq!(lhs, rhs);
            }
        }
        for s in clocks {
            for t in clocks {
                assert_eq!(
                    x.clone().retired_at(s).retired_at(t),
                    x.clone().retired_at(s.max(t))
                );
            }
        }
    }
    Ok(())
}

/// A relay inbox has no element horizon: `retired_at` is storage normalization for it.
#[test]
fn test_relay_inbox_has_no_element_horizon() -> Result<()> {
    let inbox = relay_delta(Did::from(7u32), "held", 1)?;
    assert_eq!(EntryKind::RelayMessage.element_horizon_ms(), None);
    assert_eq!(
        inbox.clone().retired_at(u128::MAX),
        inbox.try_into_storage_entry()?
    );
    Ok(())
}

/// #871 regression: a registry carrier its publisher refreshes every heartbeat (an add, then a
/// removal of the value it replaces) never expires as a carrier, yet holds one element and at
/// most the removes of the last `H + σ`, over three horizons of heartbeats.
#[test]
fn test_registry_heartbeats_keep_tombstones_bounded() -> Result<()> {
    let interval_ms: u128 = 30_000;
    let window_ms = data_horizon_ms() + TS_OFFSET_TOLERANCE_MS;
    let bound = usize::try_from(window_ms / interval_ms + 1)
        .map_err(|_| Error::InvalidMessage("tombstone bound overflows usize".to_string()))?;
    let heartbeats = 3 * window_ms / interval_ms;
    let mut carrier = empty_topic()?;
    let mut previous: Option<String> = None;
    for heartbeat in 0..heartbeats {
        let now_ms = NOW_MS + heartbeat * interval_ms;
        let descriptor = format!("descriptor@{heartbeat}");
        let delta = with_retention(
            data_entry("topic", &descriptor)?,
            now_ms + u128::from(DEFAULT_TTL_MS),
        );
        carrier = carrier.extend(now_ms, delta, actor())?;
        if let Some(replaced) = previous.replace(descriptor) {
            carrier = carrier.tombstone(data_entry("topic", &replaced)?)?;
        }
        carrier = carrier.retired_at(now_ms);
        assert!(carrier.is_live_at(now_ms));
        assert_eq!(carrier.data.len(), 1);
        assert!(carrier.crdt.tombstones.len() <= bound);
    }
    assert_eq!(carrier.crdt.tombstones.len(), bound - 1);
    Ok(())
}

/// Liveness law: a data carrier whose retention bound elapsed stays live while it holds a remove
/// that is not yet stable, so the remove outlives every add it covers; a replica whose carrier
/// other writes kept alive cannot serve the removed payload back to it.
#[test]
fn test_carrier_with_unstable_remove_outlives_its_bound() -> Result<()> {
    let horizon = data_horizon_ms();
    let bound = NOW_MS + u128::from(DEFAULT_TTL_MS);
    let short_lived = with_retention(delta_at("a", 0)?, bound);
    let removed = short_lived.tombstone(data_entry("topic", "a")?)?;
    assert!(removed.is_live_at(bound));
    let stable_at = NOW_MS + horizon + TS_OFFSET_TOLERANCE_MS;
    assert!(removed.is_live_at(stable_at - 1));
    assert!(!removed.is_live_at(stable_at));

    // Replica Q still holds the add in a carrier a later write keeps alive past `bound`.
    let refreshed = short_lived
        .join(with_retention(delta_at("b", 1_000)?, NOW_MS + 2 * horizon))?
        .retired_at(bound);
    assert_entry_data_set(&refreshed, &["a", "b"])?;
    let rejoined = removed.retired_at(bound).join(refreshed)?;
    assert_entry_data_set(&rejoined, &["b"])?;

    let inbox = with_retention(relay_delta(Did::from(7u32), "m1", 1)?, 10);
    let drained = inbox.tombstone(inbox.removal_of(inbox.crdt.dots.clone()))?;
    assert!(!drained.is_live_at(10));
    Ok(())
}

/// The directed overwrite trace: an overwrite drops the unstable remove below its register,
/// and the register then holds the carrier until it is stable, so a later sync from a replica
/// that other writes kept alive cannot serve the removed payload back. The model's
/// `test_horizon_model_catches_register_not_holding_carrier` refutes the law without it.
#[test]
fn test_overwrite_register_holds_carrier_until_stable() -> Result<()> {
    let minute: u128 = 60_000;
    let default_bound = u128::from(DEFAULT_TTL_MS);
    let a = with_retention(delta_at("a", 0)?, NOW_MS + default_bound);
    // Owner X removes `a`, then overwrites the topic with `c`.
    let removed = a.tombstone(data_entry("topic", "a")?)?;
    let overwrite_at = NOW_MS + 2 * minute;
    let replacement = with_retention(data_entry("topic", "c")?, overwrite_at + default_bound);
    let owner_x = removed.overwrite(overwrite_at, replacement, actor())?;
    assert!(owner_x.crdt.tombstones.is_empty());
    assert!(owner_x.crdt.register.is_some());
    // Replica Y still holds `a`, in a carrier a long-lived `b` keeps alive.
    let b_after_overwrite = u32::try_from(3 * minute)
        .map_err(|_| Error::InvalidMessage("offset overflows u32".to_string()))?;
    let replica_y = a.join(with_retention(
        delta_at("b", b_after_overwrite)?,
        NOW_MS + 2 * data_horizon_ms(),
    ))?;

    let after_bound = overwrite_at + default_bound + minute;
    let stored = owner_x.retired_at(after_bound);
    assert!(
        stored.is_live_at(after_bound),
        "the register holds the carrier"
    );
    assert!(stored.data.is_empty());
    let synced = stored.join(replica_y)?.retired_at(after_bound);
    assert_entry_data_set(&synced, &["b"])
}

/// Directed witness of `σ`: a remove collected at `τ + H + σ` is still held on a clock that
/// steps back by less than `σ`, so a stale add joined there stays removed. The model's
/// `test_horizon_model_catches_removes_collected_without_skew` refutes the law without `σ`.
#[test]
fn test_remove_skew_margin_survives_clock_step_back() -> Result<()> {
    let horizon = data_horizon_ms();
    let far = NOW_MS + 3 * horizon;
    let stale = with_retention(delta_at("a", 0)?, far);
    let removed = stale.tombstone(data_entry("topic", "a")?)?;
    let collected_at = NOW_MS + horizon;
    let stepped_back = collected_at - TS_OFFSET_TOLERANCE_MS / 2;
    let rejoined = removed
        .retired_at(collected_at)
        .join(stale)?
        .retired_at(stepped_back);
    assert!(rejoined.data.is_empty());
    Ok(())
}

/// Once the retention bound elapses, a carrier held live by a remove serves no
/// element, only its remove side.
#[test]
fn test_expired_bound_serves_only_the_remove_side() -> Result<()> {
    let bound = NOW_MS + u128::from(DEFAULT_TTL_MS);
    let carrier = with_retention(delta_at("a", 0)?, bound)
        .join(delta_at("b", 0)?)?
        .tombstone(data_entry("topic", "a")?)?;
    assert_entry_data_set(&carrier.clone().retired_at(bound - 1), &["b"])?;
    let expired = carrier.retired_at(bound);
    assert!(expired.is_live_at(bound));
    assert!(expired.data.is_empty());
    assert!(expired.crdt.dots.is_empty());
    assert_eq!(expired.crdt.tombstones.len(), 1);
    Ok(())
}

/// A hand-off copy whose element crossed its horizon between the copy and the ack
/// still confirms the local value, which the unprojected comparison would refuse; a write after
/// the copy does not.
#[test]
fn test_ack_confirms_across_a_horizon_crossing_but_not_a_newer_write() -> Result<()> {
    let horizon = data_horizon_ms();
    let copied_at = NOW_MS + horizon - 1;
    let acked_at = NOW_MS + horizon;
    let copy = with_retention(delta_at("a", 0)?, NOW_MS + 2 * horizon)
        .join(delta_at("b", 1_000)?)?
        .retired_at(copied_at);
    assert_entry_data_set(&copy, &["a", "b"])?;
    let ack = SyncedEntryAck::new(copy.did, copy.clone());

    // What `live_entry` reads at the ack: `a` has crossed its horizon.
    let local = copy.clone().retired_at(acked_at);
    assert_ne!(copy.clone().try_into_storage_entry()?, local);
    assert!(ack.confirms_local_value(&local, acked_at));

    let added = local.join(delta_at("c", 2_000)?)?;
    assert!(!ack.confirms_local_value(&added, acked_at));
    let removed = local.tombstone(data_entry("topic", "b")?)?;
    assert!(!ack.confirms_local_value(&removed, acked_at));
    Ok(())
}

/// A bound on the digest work over a full carrier of maximal payloads: a read
/// computes no digest whether or not anything crosses a threshold, and a join computes one
/// digest per element.
#[test]
fn test_digest_work_is_bounded_on_a_full_carrier() -> Result<()> {
    let horizon = data_horizon_ms();
    let values = (0..ENTRY_DATA_MAX_LEN)
        .map(|index| {
            let prefix = format!("{index:04}");
            let filler = "x".repeat(ENTRY_PAYLOAD_MAX_BYTES - prefix.len());
            Bytes::from(format!("{prefix}{filler}"))
        })
        .collect::<Vec<_>>();
    let mut full = Entry::new(Entry::gen_did("topic")?, values, EntryKind::Data)
        .stamp_delta(version_at(NOW_MS))?;
    full.expires_at_ms = Some(NOW_MS + 2 * horizon);
    full.crdt.tombstones = vec![EntryTombstone::of(
        &element("removed")?,
        EntryDot::for_index(version_at(NOW_MS), 0)?,
    )];
    let full = full.try_into_storage_entry()?;
    assert_eq!(full.data.len(), ENTRY_DATA_MAX_LEN);

    let digests = |operation: &dyn Fn() -> Result<Entry>| -> Result<(Entry, usize)> {
        reset_digests();
        let result = operation()?;
        Ok((result, digests_computed()))
    };
    let (untouched, hashed) = digests(&|| Ok(full.clone().retired_at(NOW_MS + 1)))?;
    assert_eq!((untouched == full, hashed), (true, 0));
    let (retired, hashed) = digests(&|| Ok(full.clone().retired_at(NOW_MS + horizon)))?;
    assert_eq!((retired.data.len(), hashed), (0, 0));
    let (joined, hashed) = digests(&|| full.join(full.clone()))?;
    assert_eq!((joined == full, hashed), (true, ENTRY_DATA_MAX_LEN));
    Ok(())
}
