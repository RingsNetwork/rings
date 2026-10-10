//! Directed witnesses of the element horizon (#867, #872, #874): per-element expiry, covering
//! removes, the skew margin, carrier liveness past the bound, and the cost of the projection.
//! The seeded model (`test_horizon_model`) checks the same laws over interleavings.

use bytes::Bytes;

use super::super::digests_computed;
use super::super::reset_digests;
use super::super::Entry;
use super::super::EntryDot;
use super::super::EntryKind;
use super::super::EntryOperation;
use super::super::EntryTombstone;
use super::super::EntryVersion;
use super::super::SyncedEntryAck;
use super::actor;
use super::assert_entry_data_set;
use super::data_entry;
use super::element;
use super::entry_dot_for_value;
use super::relay_delta;
use super::version_after;
use super::version_at;
use super::NOW_MS;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::consts::ENTRY_PAYLOAD_MAX_BYTES;
use crate::consts::MAX_TTL_MS;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::tests::with_retention;

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
                let rhs = x.clone().retired_at(t).join(y.clone().retired_at(t))?;
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

/// #871 regression: a carrier whose publisher periodically replaces its value (an add, then a
/// removal of the value it replaces) never expires as a carrier, yet holds one element and at
/// most the removes of the last `H + σ`, over three horizons of replacements.
#[test]
fn test_periodic_replacement_keeps_tombstones_bounded() -> Result<()> {
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
