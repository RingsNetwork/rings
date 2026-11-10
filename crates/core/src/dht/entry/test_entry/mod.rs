use std::collections::BTreeSet;

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
    let stale_same_actor = element("stale")?;
    let cross_actor = element("cross-actor")?;
    let live = element("live")?;
    let mut values = BTreeMap::new();
    values.insert(
        stale_same_actor.clone(),
        EntryDot::for_index(EntryVersion::new(1, register.actor, Did::from(1001u32)), 0)?,
    );
    let cross_dot = EntryDot::for_index(version(1), 0)?;
    values.insert(cross_actor.clone(), cross_dot);
    let live_dot = EntryDot::for_index(version(11), 0)?;
    values.insert(live.clone(), live_dot);

    let buffer = DataTopicBuffer::new(Some(register), values, BTreeMap::new());
    assert_eq!(buffer.values.len(), 2);
    assert!(!buffer.values.contains_key(&stale_same_actor));
    assert!(buffer.values.contains_key(&cross_actor));
    assert!(buffer.values.contains_key(&live));

    let removes = BTreeMap::from([(ElementDigest::of(&live), live_dot)]);
    let tombstoned = DataTopicBuffer::new(buffer.register, buffer.values, removes);
    assert_eq!(tombstoned.values.len(), 1);
    assert!(tombstoned.values.contains_key(&cross_actor));
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
    let issuer = actor();
    let digest = OperationDigest {
        kind: entry.kind,
        did: entry.did,
        actor: issuer,
        data: &entry.data,
    };
    let bytes = rings_codec::serialize(&digest).map_err(Error::CodecSerialize)?;

    let direct = Did::try_from(HashStr::from_bytes(&bytes))?;
    let legacy_encoded = bytes.encode()?;
    let legacy_base58 = Entry::gen_did(legacy_encoded.value())?;

    assert_eq!(entry.operation_digest(issuer)?, direct);
    assert_ne!(direct, legacy_base58);
    Ok(())
}

#[test]
fn test_forwarded_overwrite_witness_is_not_reissued_after_local_floor() -> Result<()> {
    let issuer = actor();
    let current = data_entry("topic", "current")?.stamp_overwrite(EntryVersion::new(
        10,
        issuer,
        Did::from(1010u32),
    ))?;
    let stale_forwarded = data_entry("topic", "stale")?.stamp_overwrite(EntryVersion::new(
        1,
        issuer,
        Did::from(1001u32),
    ))?;

    let updated = current.overwrite(NOW_MS, stale_forwarded, issuer)?;

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
        delta
            .clone()
            .validate_admissible_at(NOW_MS, TEST_NETWORK_ID),
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

/// Fair per-actor truncation law (#1179): one actor flooding `ENTRY_DATA_MAX_LEN` elements
/// into a shared topic cannot evict other authenticated actors' newest elements, and the
/// truncated join remains a commutative, associative, idempotent join-semilattice.
#[test]
fn test_fair_per_actor_truncation_preserves_other_writers_and_lattice_laws() -> Result<()> {
    let topic_did = Entry::gen_did("shared-topic")?;
    let honest_a = Did::from(10u32);
    let honest_b = Did::from(20u32);
    let flooder = Did::from(99u32);

    let entry_a = Entry::new(
        topic_did,
        vec![Bytes::from("honest-a-old"), Bytes::from("honest-a-new")],
        EntryKind::Data,
    )
    .stamp_delta(EntryVersion::new(100, honest_a, honest_a))?;

    let entry_b = Entry::new(topic_did, vec![Bytes::from("honest-b")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(200, honest_b, honest_b))?;

    let flood_payloads = (0..ENTRY_DATA_MAX_LEN)
        .map(|i| Bytes::from(format!("flood-{i}")))
        .collect::<Vec<_>>();
    let entry_flood = Entry::new(topic_did, flood_payloads, EntryKind::Data)
        .stamp_delta(EntryVersion::new(300, flooder, flooder))?;

    let combined = entry_a
        .clone()
        .join(entry_b.clone())?
        .join(entry_flood.clone())?;
    assert_eq!(combined.data.len(), ENTRY_DATA_MAX_LEN);
    let decoded = decode_entry_data(&combined)?;
    assert!(
        decoded.contains(&"honest-a-old".to_string()),
        "honest writer A's older item survives the flood"
    );
    assert!(
        decoded.contains(&"honest-a-new".to_string()),
        "honest writer A's newest item survives the flood"
    );
    assert!(
        decoded.contains(&"honest-b".to_string()),
        "honest writer B's item survives the flood"
    );
    assert!(
        !decoded.contains(&"flood-0".to_string())
            && !decoded.contains(&"flood-1".to_string())
            && !decoded.contains(&"flood-2".to_string()),
        "the flooder's oldest 3 items are evicted first to make room for honest writers"
    );
    assert!(
        decoded.contains(&format!("flood-{}", ENTRY_DATA_MAX_LEN - 1)),
        "the flooder's newest items are retained"
    );

    let reverse = entry_flood
        .clone()
        .join(entry_b.clone())?
        .join(entry_a.clone())?;
    let idempotent = combined
        .clone()
        .join(entry_a)?
        .join(entry_b)?
        .join(entry_flood)?;
    assert_eq!(combined, reverse);
    assert_eq!(combined, idempotent);
    Ok(())
}

/// Actor-scoped tombstone commutativity and provenance law (#1179, #1220):
/// 1. A tombstone authored by `attacker` for `ElementDigest::of(honest_value)` at a higher
///    logical timestamp (`ts = 100`) is scoped to `attacker`'s slot, so syncing in EITHER order
///    succeeds, preserves `honest`'s element (`ts = 50`), and commutes (`A ⊔ B == B ⊔ A`).
/// 2. A forged tombstone claiming `actor = honest` with an unauthenticated version is rejected by
///    `validate_data_sync_on_local`.
#[test]
fn test_cross_actor_tombstone_evicted_on_late_honest_arrival_and_rejected_when_held() -> Result<()>
{
    let topic_did = Entry::gen_did("shared-topic-out-of-order")?;
    let honest = Did::from(10u32);
    let attacker = Did::from(99u32);
    let honest_value = Bytes::from("honest-record");

    let honest_entry = EntryOperation::Extend(Entry::new(
        topic_did,
        vec![honest_value.clone()],
        EntryKind::Data,
    ))
    .stamped(50, honest)?
    .entry()
    .clone();

    let mut attacker_entry = Entry::new(
        topic_did,
        vec![Bytes::from("attacker-record")],
        EntryKind::Data,
    )
    .stamp_delta(EntryVersion::new(100, attacker, attacker))?;
    let attacker_dot = attacker_entry.crdt.dots[0];
    attacker_entry
        .crdt
        .tombstones
        .push(EntryTombstone::of(&honest_value, attacker_dot));

    // Actor-scoped tombstones allow both sync orders and commute (`A ⊔ B == B ⊔ A`) while keeping
    // `honest-record` alive.
    attacker_entry.validate_data_sync_on_local(Some(&honest_entry))?;
    honest_entry.validate_data_sync_on_local(Some(&attacker_entry))?;
    let honest_first = honest_entry.clone().join(attacker_entry.clone())?;
    let attacker_first = attacker_entry
        .without_cross_actor_tombstones_for(&honest_entry, honest)
        .join(honest_entry.clone())?;
    assert_eq!(honest_first, attacker_first);
    assert_entry_data_set(&honest_first, &["honest-record", "attacker-record"])?;

    // Forged tombstone impersonating `honest` with a synthetic operation digest is rejected.
    let mut forged_honest_tomb = EntryOperation::Extend(Entry::new(
        topic_did,
        vec![Bytes::from("attacker-cover")],
        EntryKind::Data,
    ))
    .stamped(120, attacker)?
    .entry()
    .clone();
    let forged_dot = EntryDot::for_index(EntryVersion::new(120, honest, Did::from(9999u32)), 0)?;
    forged_honest_tomb
        .crdt
        .tombstones
        .push(EntryTombstone::of(&honest_value, forged_dot));
    assert!(matches!(
        forged_honest_tomb.validate_data_sync_on_local(Some(&honest_entry)),
        Err(Error::DataEntryWriteUnauthorized)
    ));
    Ok(())
}

/// Shared-payload join associativity and co-author provenance law (#1179, #1220):
/// 1. Joining a 1,024-item flood by `actor_a` (containing `"shared"` at `t = 100`), a write of
///    `"shared"` by `actor_b` (`t = 200`), and a newer write `"a-newer"` by `actor_a` (`t = 300`)
///    is associative across `(flood ⊔ b) ⊔ a_newer` and `flood ⊔ (b ⊔ a_newer)`.
/// 2. Accumulated replicated states (`Overwrite` by `actor_a` followed by `Extend` by `actor_b`,
///    and `Overwrite` of 1 item by `actor_a` followed by `Tombstone` by `actor_a`) pass
///    `validate_data_sync_on_local`.
/// 3. Co-authors can independently append and retract their own co-author slot on a shared payload
///    without locking each other out or deleting another co-author's live payload.
#[test]
fn test_shared_payload_join_associativity_accumulated_register_sync_and_payload_copy_defense(
) -> Result<()> {
    let topic_did = Entry::gen_did("shared-payload-associativity")?;
    let actor_a = Did::from(10u32);
    let actor_b = Did::from(20u32);

    let mut flood_items = vec![Bytes::from("shared")];
    flood_items.extend((1..ENTRY_DATA_MAX_LEN).map(|i| Bytes::from(format!("flood-{i}"))));
    let flood_a = Entry::new(topic_did, flood_items, EntryKind::Data)
        .stamp_delta(EntryVersion::new(100, actor_a, actor_a))?;
    let shared_b = Entry::new(topic_did, vec![Bytes::from("shared")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(200, actor_b, actor_b))?;
    let newer_a = Entry::new(topic_did, vec![Bytes::from("a-newer")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(300, actor_a, actor_a))?;

    let left_assoc = flood_a
        .clone()
        .join(shared_b.clone())?
        .join(newer_a.clone())?;
    let right_assoc = flood_a.join(shared_b.clone().join(newer_a)?)?;
    assert_eq!(left_assoc, right_assoc);
    assert!(left_assoc.data.contains(&Bytes::from("shared")));
    assert!(left_assoc.data.contains(&Bytes::from("a-newer")));

    // Accumulated state 1: Overwrite by A at t=100, then Extend by B at t=200.
    let overwrite_a = EntryOperation::Overwrite(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(100, actor_a)?;
    let base_a = overwrite_a.entry().clone();
    let extend_b = EntryOperation::Extend(Entry::new(
        topic_did,
        vec![Bytes::from("b-ext")],
        EntryKind::Data,
    ))
    .stamped(200, actor_b)?;
    let accumulated_ab = base_a.clone().operate(200, extend_b, actor_b)?;
    accumulated_ab.validate_data_sync_on_local(None)?;
    accumulated_ab.validate_data_sync_on_local(Some(&base_a))?;
    base_a.validate_data_sync_on_local(Some(&accumulated_ab))?;

    // Accumulated state 2: Overwrite of 1 item by A at t=100, then Tombstone by A at t=150.
    let tomb_a = EntryOperation::Tombstone(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(150, actor_a)?;
    let emptied_a = base_a.clone().operate(150, tomb_a, actor_a)?;
    assert!(emptied_a.data.is_empty());
    assert!(!emptied_a.crdt.tombstones.is_empty());
    emptied_a.validate_data_sync_on_local(None)?;
    emptied_a.validate_data_sync_on_local(Some(&base_a))?;

    // Unauthorized direct retraction: B cannot Tombstone A's "a-base" without holding a slot.
    let unauthorized_tomb_b = EntryOperation::Tombstone(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(240, actor_b)?;
    assert!(matches!(
        unauthorized_tomb_b.validate_data_write_on_local(Some(&base_a), actor_b),
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // Co-author Extend & independent slot retraction (#1220): B CAN Extend("a-base") alongside A,
    // and when B later Tombstones "a-base", only B's co-dot is retracted while A's "a-base" lives!
    let co_extend_b = EntryOperation::Extend(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(250, actor_b)?;
    co_extend_b.validate_data_write_on_local(Some(&base_a), actor_b)?;
    let co_held = base_a.clone().operate(250, co_extend_b, actor_b)?;
    assert_eq!(co_held.crdt.co_dots.len(), 1);

    let retract_b = EntryOperation::Tombstone(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(260, actor_b)?;
    retract_b.validate_data_write_on_local(Some(&co_held), actor_b)?;
    let after_b_retract = co_held.operate(260, retract_b, actor_b)?;
    assert_entry_data_set(&after_b_retract, &["a-base"])?;
    assert!(after_b_retract.crdt.co_dots.is_empty());

    // Once B's slot is retracted, B cannot Tombstone A's remaining "a-base".
    let repeat_tomb_b = EntryOperation::Tombstone(Entry::new(
        topic_did,
        vec![Bytes::from("a-base")],
        EntryKind::Data,
    ))
    .stamped(270, actor_b)?;
    assert!(matches!(
        repeat_tomb_b.validate_data_write_on_local(Some(&after_b_retract), actor_b),
        Err(Error::DataEntryWriteUnauthorized)
    ));
    Ok(())
}

/// Issue #1220 Defect 1: unattributed witnesses (`actor == Did::default()`) never satisfy
/// `has_write_witness()` or `is_authored_by()`, are rejected by both write and sync validation,
/// and are forcibly stamped with `actor` by `ensure_stamp_after` (preserving any stray `register`
/// on `Extend` so `has_delta_envelope()` still rejects malformed deltas).
#[test]
fn test_issue_1220_unattributed_witness_rejected_and_stamped_by_ensure_stamp_after() -> Result<()> {
    let topic_did = Entry::gen_did("issue-1220-unattributed")?;
    let actor_a = Did::from(10u32);
    let actor_b = Did::from(20u32);

    let unattributed = EntryVersion {
        logical_time_ms: NOW_MS,
        counter: 3,
        actor: Did::default(),
        operation: Did::from(777u32),
    };
    assert!(!unattributed.is_authored_by(Did::default()));
    assert!(!unattributed.is_authored_by(actor_a));

    // Unattributed Overwrite with non-default `operation` and `counter != 0` has no write witness.
    let mut raw_overwrite = Entry::new(topic_did, vec![Bytes::from("spoof")], EntryKind::Data);
    raw_overwrite.expires_at_ms = Some(NOW_MS + 60_000);
    raw_overwrite.crdt.register = Some(unattributed);
    raw_overwrite.crdt.dots = vec![EntryDot::for_index(unattributed, 0)?];
    assert!(!raw_overwrite.crdt.has_write_witness());

    assert!(matches!(
        raw_overwrite.validate_data_sync_on_local(None),
        Err(Error::DataEntryWriteUnauthorized)
    ));
    let raw_op = EntryOperation::Overwrite(raw_overwrite);
    assert!(matches!(
        raw_op.validate_data_write(
            NOW_MS,
            TEST_NETWORK_ID,
            actor_b,
            crate::message::Principal::Unbound
        ),
        Err(Error::DataEntryWriteUnauthorized)
    ));
    assert!(matches!(
        raw_op.validate_data_write(
            NOW_MS,
            TEST_NETWORK_ID,
            Did::default(),
            crate::message::Principal::Local
        ),
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // `stamped(..., actor_b)` stamps `actor_b` onto `register` and `dots`, so it cannot masquerade
    // as an unattributed carrier to overwrite `actor_a`'s live state.
    let live_a = EntryOperation::Overwrite(Entry::new(
        topic_did,
        vec![Bytes::from("a-owned")],
        EntryKind::Data,
    ))
    .stamped(NOW_MS - 10, actor_a)?
    .entry()
    .clone();
    let stamped_b = raw_op.stamped(NOW_MS, actor_b)?;
    assert!(stamped_b
        .entry()
        .versions()
        .all(|v| v.is_authored_by(actor_b)));
    assert!(matches!(
        stamped_b.validate_data_write_on_local(Some(&live_a), actor_b),
        Err(Error::DataEntryWriteUnauthorized)
    ));

    // Malformed `Extend` carrying a stray unattributed `register` keeps `register` stamped with
    // `actor_b` so `has_delta_envelope()` rejects it.
    let mut bad_extend = Entry::new(topic_did, vec![Bytes::from("bad-delta")], EntryKind::Data);
    bad_extend.crdt.register = Some(unattributed);
    let stamped_bad_extend = EntryOperation::Extend(bad_extend).stamped(NOW_MS, actor_b)?;
    assert!(stamped_bad_extend.entry().crdt.register.is_some());
    assert!(matches!(
        stamped_bad_extend.validate_data_write(
            NOW_MS,
            TEST_NETWORK_ID,
            actor_b,
            crate::message::Principal::Unbound
        ),
        Err(Error::DataEntryWriteUnauthorized)
    ));
    Ok(())
}

/// Issue #1220 Defect 5: water-filling eviction (`retain_fair_data_elements`) operates over unique
/// `(Bytes, primary_dot)` values rather than flattened co-dots, so co-author dots on shared
/// payloads never consume extra `ENTRY_DATA_MAX_LEN` slots and `retire_t(A ⊔ B) = retire_t(A) ⊔ retire_t(B)`
/// holds even when co-dots cross the element horizon at capacity.
#[test]
fn test_issue_1220_unique_value_water_filling_resists_co_dot_amplification_and_commutes_with_retire(
) -> Result<()> {
    let topic_did = Entry::gen_did("issue-1220-waterfill")?;
    let honest = Did::from(10u32);
    let flooder = Did::from(99u32);

    // 10 sybil co-authors all append the exact same 5 shared payloads.
    let shared_payloads = (0..5)
        .map(|i| Bytes::from(format!("shared-{i}")))
        .collect::<Vec<_>>();
    let mut acc = Entry::new(
        topic_did,
        vec![Bytes::from("honest-1"), Bytes::from("honest-2")],
        EntryKind::Data,
    )
    .stamp_delta(EntryVersion::new(NOW_MS + 100, honest, honest))?;

    for sybil_idx in 0..10u32 {
        let sybil = Did::from(200 + sybil_idx);
        let sybil_delta =
            Entry::new(topic_did, shared_payloads.clone(), EntryKind::Data).stamp_delta(
                EntryVersion::new(NOW_MS + 150 + u128::from(sybil_idx), sybil, sybil),
            )?;
        acc = acc.join(sybil_delta)?;
    }
    // 2 honest + 5 shared = 7 unique elements, NOT 52!
    assert_eq!(acc.data.len(), 7);
    assert_eq!(acc.crdt.co_dots.len(), 5 * 9);

    let flood_payloads = (0..ENTRY_DATA_MAX_LEN)
        .map(|i| Bytes::from(format!("flood-{i}")))
        .collect::<Vec<_>>();
    let flood_delta = Entry::new(topic_did, flood_payloads, EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 300, flooder, flooder))?;
    let combined = acc.join(flood_delta)?;

    // Full `ENTRY_DATA_MAX_LEN` unique values are retained, including honest writer's items and all
    // 5 shared items (the flooder loses 7 oldest items, NOT 52).
    assert_eq!(combined.data.len(), ENTRY_DATA_MAX_LEN);
    let decoded = decode_entry_data(&combined)?;
    assert!(decoded.contains(&"honest-1".to_string()));
    assert!(decoded.contains(&"honest-2".to_string()));
    for i in 0..5 {
        assert!(decoded.contains(&format!("shared-{i}")));
    }
    assert!(!decoded.contains(&"flood-6".to_string()));
    assert!(decoded.contains(&"flood-7".to_string()));

    // Homomorphism `retire_t(A ⊔ B) = retire_t(A) ⊔ retire_t(B)` at capacity when a shared value has
    // an older co-dot (`t0 = NOW_MS + 100`) and a newer primary dot (`t1 = NOW_MS + 300`).
    let law = retention::ElementRetention::of(EntryKind::Data)
        .ok_or_else(|| Error::InvalidMessage("Data has retention law".to_string()))?;
    let actor_a = Did::from(11u32);
    let actor_b = Did::from(12u32);
    let actor_c = Did::from(13u32);

    let a_shared = Entry::new(topic_did, vec![Bytes::from("shared")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 100, actor_a, actor_a))?;
    let a_fill = Entry::new(
        topic_did,
        (0..(ENTRY_DATA_MAX_LEN - 1))
            .map(|i| Bytes::from(format!("fill-{i}")))
            .collect(),
        EntryKind::Data,
    )
    .stamp_delta(EntryVersion::new(NOW_MS + 200, actor_a, actor_a))?;
    let c_extra = Entry::new(topic_did, vec![Bytes::from("c-extra")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 250, actor_c, actor_c))?;
    let carrier_a = a_shared.join(a_fill)?.join(c_extra)?;

    let carrier_b = Entry::new(topic_did, vec![Bytes::from("shared")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 300, actor_b, actor_b))?;

    for check_ms in [
        NOW_MS + 100 + law.add_horizon_ms - 1,
        NOW_MS + 100 + law.add_horizon_ms,
        NOW_MS + 200 + law.add_horizon_ms,
    ] {
        let lhs = carrier_a
            .clone()
            .join(carrier_b.clone())?
            .horizon_retired_under(law, check_ms);
        let rhs = carrier_a
            .clone()
            .horizon_retired_under(law, check_ms)
            .join(carrier_b.clone().horizon_retired_under(law, check_ms))?;
        assert_eq!(lhs, rhs, "retire_t homomorphism failed at {check_ms}");
    }

    // When `values.len() > max_len` and an admitted shared value has co-dots from multiple
    // honest writers (`actor_c` at `NOW_MS + 100` and `actor_d` at `NOW_MS + 350`, both at rank 0)
    // alongside a flooder (`actor_a` writing `ENTRY_DATA_MAX_LEN` items at `NOW_MS + 400`), both
    // co-authors' dots are preserved, the primary dot is the newest (`NOW_MS + 350`), `retire_t`
    // commutes with `join`, and `join` is associative.
    let actor_d = Did::from(14u32);
    let low_vol_entries = Entry::new(topic_did, vec![Bytes::from("b-unique")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 250, actor_b, actor_b))?
        .join(
            Entry::new(topic_did, vec![Bytes::from("co-shared")], EntryKind::Data)
                .stamp_delta(EntryVersion::new(NOW_MS + 100, actor_c, actor_c))?,
        )?;
    let high_vol_shared = Entry::new(topic_did, vec![Bytes::from("co-shared")], EntryKind::Data)
        .stamp_delta(EntryVersion::new(NOW_MS + 350, actor_d, actor_d))?;
    let high_vol_flood = Entry::new(
        topic_did,
        (0..ENTRY_DATA_MAX_LEN)
            .map(|i| Bytes::from(format!("hv-{i}")))
            .collect(),
        EntryKind::Data,
    )
    .stamp_delta(EntryVersion::new(NOW_MS + 400, actor_a, actor_a))?;

    let joined_lh = low_vol_entries
        .clone()
        .join(high_vol_shared.clone())?
        .join(high_vol_flood.clone())?;
    let joined_hl = low_vol_entries
        .clone()
        .join(high_vol_shared.clone().join(high_vol_flood.clone())?)?;
    assert_eq!(joined_lh, joined_hl);
    let shared_pos = joined_lh
        .data
        .iter()
        .position(|v| v == &Bytes::from("co-shared"))
        .ok_or_else(|| Error::InvalidMessage("co-shared missing".to_string()))?;
    assert_eq!(
        joined_lh.crdt.dots[shared_pos].version.logical_time_ms,
        NOW_MS + 350,
        "primary dot of admitted co-authored value must not regress to low-volume timestamp"
    );
    assert!(
        joined_lh
            .crdt
            .co_dots
            .iter()
            .any(|&(idx, d)| idx as usize == shared_pos && d.version.actor == actor_c),
        "co-dot of admitted value must be preserved past max_len cutoff"
    );

    let check_ms = NOW_MS + 100 + law.add_horizon_ms;
    let lhs_retire = low_vol_entries
        .clone()
        .join(high_vol_shared.clone())?
        .join(high_vol_flood.clone())?
        .horizon_retired_under(law, check_ms);
    let rhs_retire = low_vol_entries
        .horizon_retired_under(law, check_ms)
        .join(high_vol_shared.horizon_retired_under(law, check_ms))?
        .join(high_vol_flood.horizon_retired_under(law, check_ms))?;
    assert_eq!(lhs_retire, rhs_retire);

    // Sentinel counter reservation in `EntryVersion::after` and relay inbox CRDT rejection.
    let near_sentinel_floor = EntryVersion {
        logical_time_ms: NOW_MS,
        counter: crdt::PROOF_CHUNK_SENTINEL_COUNTER - 1,
        actor: actor_a,
        operation: actor_a,
    };
    assert!(matches!(
        EntryVersion::new(NOW_MS, actor_a, actor_a).after(Some(near_sentinel_floor)),
        Err(Error::EntryVersionAheadOfClock)
    ));
    let mut relay_with_co = Entry::new(topic_did, Vec::new(), EntryKind::RelayMessage);
    relay_with_co.crdt.co_registers.push(near_sentinel_floor);
    assert!(matches!(
        relay_with_co.witnessed_inbox_elements(NOW_MS, TEST_NETWORK_ID),
        Err(Error::RelayInboxRegisterNotAllowed)
    ));
    Ok(())
}

mod test_horizon;
