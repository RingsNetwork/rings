//! Storage reads under the element horizon (#867, #872): a carrier past its retention bound
//! answers lookups as absent from storage and from the fetch cache, a read writes a retiring
//! projection back, and the digest work of the storage paths is bounded.

use bytes::Bytes;

use super::super::super::chord::PeerRing;
use super::super::super::chord::PeerRingAction;
use super::data_entry_with_data;
use crate::dht::entry::digests_computed;
use crate::dht::entry::reset_digests;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryTombstone;
use crate::dht::entry::EntryVersion;
use crate::dht::entry::PlacementMiss;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::Principal;
use crate::storage::MemStorage;
use crate::tests::live_entry;
use crate::utils::get_epoch_ms;

/// The elements of the carrier the digest-bound test stores.
const DIGEST_BOUND_ELEMENTS: usize = 256;

/// A live data carrier of [`DIGEST_BOUND_ELEMENTS`] elements issued now, with one remove, so
/// normalization hashes every element.
fn carrier_with_a_remove(did: Did, now_ms: u128) -> Result<Entry> {
    let version = EntryVersion::new(now_ms, Did::from(1u32), Did::from(2u32));
    let dot = |index: usize| -> Result<EntryDot> {
        let index = u32::try_from(index)
            .map_err(|_| Error::InvalidMessage("element index overflows u32".to_string()))?;
        Ok(EntryDot { version, index })
    };
    let mut entry = live_entry(
        did,
        (0..DIGEST_BOUND_ELEMENTS)
            .map(|index| Bytes::from(format!("element-{index}")))
            .collect(),
        EntryKind::Data,
    );
    entry.crdt.dots = (0..DIGEST_BOUND_ELEMENTS)
        .map(dot)
        .collect::<Result<Vec<_>>>()?;
    entry.crdt.tombstones = vec![EntryTombstone::of(&Bytes::from("removed"), dot(0)?)];
    entry.try_into_storage_entry()
}

/// Run `operation` and count the element digests it computes on this thread.
async fn digests_of<T>(operation: impl std::future::Future<Output = Result<T>>) -> Result<usize> {
    reset_digests();
    operation.await?;
    Ok(digests_computed())
}

/// A bound on the digest work of the production storage paths under the storage
/// transition: a sync join hashes each element once, into an empty slot and into a held one
/// alike, and the ack comparison of a hand-off hashes none.
#[tokio::test(flavor = "current_thread")]
async fn test_storage_join_and_ack_digest_each_element_at_most_once() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let placement_key = Did::from(100u32);
    let now_ms = get_epoch_ms();
    let carrier = carrier_with_a_remove(Did::from(10u32), now_ms)?;

    let into_empty = digests_of(node.join_storage_entry(
        now_ms,
        placement_key,
        carrier.clone(),
        Principal::Local,
    ));
    assert_eq!(into_empty.await?, DIGEST_BOUND_ELEMENTS);
    let into_held = digests_of(node.join_storage_entry(
        now_ms,
        placement_key,
        carrier.clone(),
        Principal::Local,
    ));
    assert_eq!(into_held.await?, DIGEST_BOUND_ELEMENTS);

    let key = StorageKey::new(EntryKind::Data, placement_key);
    let incarnation = node.observe_storage_handoff_incarnation(key)?;
    let ack = SyncedEntryAck::new(placement_key, carrier).with_custody_incarnation(incarnation);
    let confirmed = digests_of(node.remove_storage_entry_confirmed_by(key, now_ms, &ack));
    assert_eq!(confirmed.await?, 0);
    assert_eq!(node.storage.count().await?, 0);
    Ok(())
}

/// A drained data carrier holding one remove issued a second ago, with retention bound
/// `expires_at_ms`.
fn drained_carrier(did: Did, now_ms: u128, expires_at_ms: u128) -> Entry {
    let mut entry = Entry::new(did, vec![], EntryKind::Data);
    entry.crdt.tombstones = vec![EntryTombstone::of(&Bytes::from("removed"), EntryDot {
        version: EntryVersion::new(now_ms - 1_000, Did::from(1u32), Did::from(2u32)),
        index: 0,
    })];
    entry.expires_at_ms = Some(expires_at_ms);
    entry
}

/// A read that retires part of a stored carrier writes the projection back, so
/// retired payload bytes stop occupying storage while the carrier's remove side holds it live.
#[tokio::test]
async fn test_read_writes_back_a_projection_that_retired_elements() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let key = Did::from(100u32);
    let mut stored = drained_carrier(key, now_ms, now_ms - 1);
    stored.data = vec![Bytes::from("expired")];
    stored.crdt.dots = vec![EntryDot {
        version: EntryVersion::new(now_ms - 1_000, Did::from(3u32), Did::from(4u32)),
        index: 0,
    }];
    node.storage.put(&key.to_string(), &stored).await?;

    let read = node
        .live_storage_entry(StorageKey::new(EntryKind::Data, key), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("the remove holds the carrier".to_string()))?;
    assert!(read.data.is_empty());
    // The write-back factors the carrier (#920): no element side is left holding the retired
    // payload, and the glued read is the projection.
    let element_side = node.storage.get(&key.to_string()).await?;
    assert!(
        element_side.is_some_and(|side| side.data.is_empty() && side.crdt.tombstones.is_empty())
    );
    assert_eq!(
        node.live_storage_entry(StorageKey::new(EntryKind::Data, key), now_ms)
            .await?,
        Some(read)
    );
    Ok(())
}

/// A cached carrier past its retention bound, held live only by a remove, is
/// served by the fetch cache as absent, as a replica serves it, while a drained carrier inside
/// its bound is still served, so its removes reach the reader.
#[tokio::test]
async fn test_cache_serves_a_carrier_past_its_bound_as_absent() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let expired_key = Did::from(100u32);
    let live_key = Did::from(200u32);
    node.local_cache_put(drained_carrier(expired_key, now_ms, now_ms - 1))
        .await?;
    node.local_cache_put(drained_carrier(live_key, now_ms, now_ms + 60_000))
        .await?;

    assert_eq!(node.local_cache_get(expired_key).await?, None);
    // Read-repair of a missed placement still reads the held carrier, whose removes it spreads.
    assert!(node
        .local_cache_held(expired_key, get_epoch_ms())
        .await?
        .is_some());
    let live = node
        .local_cache_get(live_key)
        .await?
        .ok_or_else(|| Error::InvalidMessage("an in-bound carrier is served".to_string()))?;
    assert!(live.data.is_empty());
    assert_eq!(live.crdt.tombstones.len(), 1);
    Ok(())
}

/// When the first placement holds a carrier past its bound, held live only by an unstable
/// remove, the lookup does not stop there but asks the next placement, which answers with its
/// data; the first placement is reported missed for read-repair, and its carrier is kept. A
/// drained carrier inside its bound still answers as found, so its removes reach the reader's
/// cache.
#[tokio::test]
async fn test_lookup_moves_past_an_expired_placement_to_the_next() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let resource = Did::from(100u32);
    let placements = resource.rotate_affine(2)?;
    let (Some(expired), Some(live)) = (placements.first().copied(), placements.get(1).copied())
    else {
        return Err(Error::InvalidMessage("two placements".to_string()));
    };
    node.storage
        .put(
            &expired.to_string(),
            &drained_carrier(resource, now_ms, now_ms - 1),
        )
        .await?;
    node.storage
        .put(
            &live.to_string(),
            &data_entry_with_data(resource, "descriptor"),
        )
        .await?;

    let PeerRingAction::SomeEntry(evidence) = node.entry_lookup(resource, 2).await? else {
        return Err(Error::InvalidMessage(
            "the live placement answers".to_string(),
        ));
    };
    assert_eq!(evidence.entry.data, vec![Bytes::from("descriptor")]);
    assert_eq!(evidence.misses, vec![PlacementMiss::new(expired, node.did)]);
    assert!(node.storage.get(&expired.to_string()).await?.is_some());

    let in_bound = Did::from(200u32);
    node.storage
        .put(
            &in_bound.to_string(),
            &drained_carrier(in_bound, now_ms, now_ms + 60_000),
        )
        .await?;
    assert!(matches!(
        node.entry_lookup(in_bound, 1).await?,
        PeerRingAction::SomeEntry(evidence) if evidence.entry.data.is_empty()
    ));
    Ok(())
}

/// #1332 regression: `PeerRing::read_epoch_ms` and `read_epoch_ms_for` fence backward/stalled
/// wall clocks via the monotonic floor (including when `SystemClock` is injected via `with_clock`,
/// as `SwarmBuilder::build` does) and the Byzantine-safe admitted HLC floor so retired additions
/// cannot resurface on local reads, cache lookups, or DHT lookups.
#[tokio::test(flavor = "current_thread")]
async fn test_peer_ring_read_epoch_fences_lagging_clock_and_prevents_resurrection() -> Result<()> {
    use std::sync::Arc;

    use crate::consts::MAX_TTL_MS;
    use crate::consts::TS_OFFSET_TOLERANCE_MS;
    use crate::dht::entry::EntryDot;
    use crate::dht::entry::EntryVersion;
    use crate::dht::ChordStorageCache;
    use crate::tests::with_retention;
    use crate::utils::ManualClock;
    use crate::utils::SystemClock;

    let t0 = 1_700_000_000_000_u128;
    let horizon = u128::from(MAX_TTL_MS);
    let t1 = t0 + 20_000;
    let t_later = t0 + horizon + 4_000;
    let clock = Arc::new(ManualClock::new(t0));
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()))
        .with_clock(clock.clone());

    let resource = Did::from(300u32);
    let mut stale_cached = with_retention(
        Entry::new(resource, vec![Bytes::from("stale")], EntryKind::Data),
        t0 + horizon,
    );
    stale_cached.crdt.dots = vec![EntryDot {
        version: EntryVersion::new(t0, Did::from(1u32), Did::from(10u32)),
        index: 0,
    }];
    node.local_cache_put(stale_cached.clone()).await?;

    clock.set(t1);
    let mut keep_entry = with_retention(
        Entry::new(resource, vec![Bytes::from("keep")], EntryKind::Data),
        t1 + horizon,
    );
    keep_entry.crdt.dots = vec![EntryDot {
        version: EntryVersion::new(t1, Did::from(2u32), Did::from(20u32)),
        index: 0,
    }];
    node.local_cache_put(keep_entry.clone()).await?;

    let placement = resource
        .rotate_affine(1)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::InvalidMessage("one placement".to_string()))?;
    let combined = stale_cached.join(keep_entry)?;
    node.storage.put(&placement.to_string(), &combined).await?;

    // Peer `node` later admits a legitimate entry at `t_later`, setting `HLC floor = t_later - σ`.
    clock.set(t_later);
    let other_key = Did::from(301u32);
    let mut fresh = with_retention(
        Entry::new(other_key, vec![Bytes::from("fresh")], EntryKind::Data),
        t_later + 60_000,
    );
    fresh.crdt.dots = vec![EntryDot {
        version: EntryVersion::new(t_later, Did::from(3u32), Did::from(30u32)),
        index: 0,
    }];
    node.join_storage_entry(t_later, other_key, fresh, Principal::Local)
        .await?;

    // Backward wall-clock rollback 10 s behind `t_later` (< `t0 + H`) stays fenced at `t_later - σ`.
    clock.set(t_later - 10_000);
    assert_eq!(node.read_epoch_ms(), t_later - TS_OFFSET_TOLERANCE_MS);
    let fenced_read_ms = node.read_epoch_ms_for(t_later - 10_000);
    assert_eq!(fenced_read_ms, t_later - TS_OFFSET_TOLERANCE_MS);

    // Fetch cache read strips `"stale"` at the fenced timestamp while keeping `"keep"`.
    let cached = node
        .local_cache_get(resource)
        .await?
        .ok_or_else(|| Error::InvalidMessage("expected live cached carrier".to_string()))?;
    assert_eq!(cached.data, vec![Bytes::from("keep")]);

    // Handler-style explicit `read_epoch_ms_for(lagged_wall)` also fences `local_cache_get_at` and
    // `local_cache_held`.
    let cached_for = node
        .local_cache_get_at(resource, fenced_read_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("expected fenced cached carrier".to_string()))?;
    assert_eq!(cached_for.data, vec![Bytes::from("keep")]);
    let held_for = node
        .local_cache_held(resource, fenced_read_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("expected fenced held carrier".to_string()))?;
    assert_eq!(held_for.data, vec![Bytes::from("keep")]);

    // Storage lookup also strips `"stale"` rather than resurface it under the lagging clock.
    let PeerRingAction::SomeEntry(evidence) = node.entry_lookup(resource, 1).await? else {
        return Err(Error::InvalidMessage("expected stored carrier".to_string()));
    };
    assert_eq!(evidence.entry.data, vec![Bytes::from("keep")]);

    // `SwarmBuilder::build()` injects `Arc::new(SystemClock)` via `with_clock`; verify monotonic
    // fencing stays enabled (`>= t0` after backward step), unlike deterministic `ManualClock`.
    let system_injected =
        PeerRing::new_with_storage(Did::from(4u32), 3, Box::new(MemStorage::new()))
            .with_clock(Arc::new(SystemClock));
    assert_eq!(system_injected.read_epoch_ms_for(t0), t0);
    assert!(system_injected.read_epoch_ms_for(t0 - 5_000) >= t0);
    assert_eq!(
        system_injected.read_epoch_ms_with(t0 + 1_000, Some(horizon + 1_000)),
        t0 + horizon + 1_000
    );

    let manual_injected =
        PeerRing::new_with_storage(Did::from(5u32), 3, Box::new(MemStorage::new()))
            .with_clock(Arc::new(ManualClock::new(t0)));
    assert_eq!(manual_injected.read_epoch_ms_for(t0), t0);
    assert_eq!(manual_injected.read_epoch_ms_for(t0 - 5_000), t0 - 5_000);
    Ok(())
}
