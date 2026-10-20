//! Fetched-entry cache laws: admission and retention shared with the replica store, and the
//! read-join (#864) under which replies cache to their join in any order.

use bytes::Bytes;

use super::super::super::chord::PeerRing;
use super::data_entry;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::EntryVersion;
use crate::dht::ChordStorageCache;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;
use crate::storage::MemStorage;
use crate::tests::expired;
use crate::tests::with_retention;
use crate::utils::get_epoch_ms;

/// The fetched-entry cache shares the admission law, and a read reports an expired value absent
/// without removing it, since a removal could undo a concurrent put; a later put replaces it.
#[tokio::test]
async fn test_local_cache_shares_admission_and_retention() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let resource = Did::from(10u32);

    assert!(matches!(
        node.local_cache_put(expired(data_entry(resource))).await,
        Err(Error::EntryNotLive)
    ));
    let mut pinned = data_entry(resource);
    pinned.crdt.register = Some(EntryVersion::new(
        u128::MAX,
        Did::from(9u32),
        Did::from(1u32),
    ));
    assert!(matches!(
        node.local_cache_put(pinned).await,
        Err(Error::EntryVersionAheadOfClock)
    ));

    node.cache
        .put(&resource.to_string(), &expired(data_entry(resource)))
        .await?;
    assert_eq!(node.local_cache_get(resource).await?, None);
    assert_eq!(node.cache.count().await?, 1);
    let fresh = data_entry(resource);
    node.local_cache_put(fresh).await?;
    assert!(node.local_cache_get(resource).await?.is_some());
    assert_eq!(node.cache.count().await?, 1);
    Ok(())
}

/// The replica shape of `operation` on `value` for the carrier `did`, stamped by `writer` at
/// `now_ms`. The carrier is undotted, with a live retention bound, so the stamp issues its
/// dots and register under `writer`, as a replica's write does.
fn stamped_delta(
    operation: fn(Entry) -> EntryOperation,
    did: Did,
    value: &str,
    now_ms: u128,
    writer: Did,
) -> Result<Entry> {
    let carrier = with_retention(
        Entry::new(
            did,
            vec![Bytes::copy_from_slice(value.as_bytes())],
            EntryKind::Data,
        ),
        now_ms + 60 * 60 * 1_000,
    );
    Ok(operation(carrier).stamped(now_ms, writer)?.entry().clone())
}

/// A stamped `Extend` delta that adds `value` to the carrier `did`, written by `writer` at
/// `now_ms`: the shape a replica holds after that write.
fn extend_delta(did: Did, value: &str, now_ms: u128, writer: Did) -> Result<Entry> {
    stamped_delta(EntryOperation::Extend, did, value, now_ms, writer)
}

/// The carrier cached for `resource` after `replies` are put in order into a fresh cache.
async fn cached_after(resource: Did, replies: &[&Entry]) -> Result<Option<Entry>> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    for reply in replies {
        node.local_cache_put((*reply).clone()).await?;
    }
    node.local_cache_get(resource).await
}

/// Assert that `first` and `second` cache to the same carrier in both reply orders, repeats
/// included, and return it.
async fn assert_cached_in_any_order(
    resource: Did,
    first: &Entry,
    second: &Entry,
) -> Result<Option<Entry>> {
    let forward = cached_after(resource, &[first, second, first]).await?;
    let reverse = cached_after(resource, &[second, first, second]).await?;
    assert_eq!(forward, reverse);
    Ok(forward)
}

/// Read-join (#864): replies adding distinct payloads cache to their union in either order.
#[tokio::test]
async fn test_local_cache_joins_replies_in_any_order() -> Result<()> {
    let resource = Did::from(10u32);
    let now_ms = get_epoch_ms();
    let first = extend_delta(resource, "a", now_ms, Did::from(1u32))?;
    let second = extend_delta(resource, "b", now_ms, Did::from(2u32))?;

    let cached = assert_cached_in_any_order(resource, &first, &second).await?;
    assert_eq!(cached.map(|entry| entry.data.len()), Some(2));
    Ok(())
}

/// Tie (#864): one payload added by two writers at the same instant competes on its dot, and
/// the cache keeps the greater dot whichever reply arrives last.
#[tokio::test]
async fn test_local_cache_breaks_a_same_instant_add_tie_by_dot() -> Result<()> {
    let resource = Did::from(10u32);
    let now_ms = get_epoch_ms();
    let first = extend_delta(resource, "a", now_ms, Did::from(1u32))?;
    let second = extend_delta(resource, "a", now_ms, Did::from(2u32))?;
    assert_ne!(first.crdt.dots, second.crdt.dots);

    let cached = assert_cached_in_any_order(resource, &first, &second).await?;
    let greater = first
        .crdt
        .dots
        .iter()
        .chain(&second.crdt.dots)
        .max()
        .copied();
    assert_eq!(
        cached.map(|entry| entry.crdt.dots.last().copied()),
        Some(greater)
    );
    Ok(())
}

/// Tie (#864): two overwrites by different writers at the same instant compete on the
/// register, and the cache keeps the greater register's value whichever arrives last.
#[tokio::test]
async fn test_local_cache_breaks_a_same_instant_overwrite_tie_by_register() -> Result<()> {
    let resource = Did::from(10u32);
    let now_ms = get_epoch_ms();
    let first = stamped_delta(
        EntryOperation::Overwrite,
        resource,
        "a",
        now_ms,
        Did::from(1u32),
    )?;
    let second = stamped_delta(
        EntryOperation::Overwrite,
        resource,
        "b",
        now_ms,
        Did::from(2u32),
    )?;
    assert_ne!(first.crdt.register, second.crdt.register);

    let cached = assert_cached_in_any_order(resource, &first, &second).await?;
    let winner = if first.crdt.register > second.crdt.register {
        &first
    } else {
        &second
    };
    assert_eq!(
        cached.map(|entry| (entry.crdt.register, entry.data)),
        Some((winner.crdt.register, winner.data.clone()))
    );
    Ok(())
}

/// Tombstones propagate through the cache: a stale replica that still holds a removed element
/// does not resurrect it when it answers after the replica that observed the removal.
#[tokio::test]
async fn test_local_cache_keeps_a_tombstone_against_a_stale_reply() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let resource = Did::from(10u32);
    let added = extend_delta(resource, "a", get_epoch_ms(), Did::from(1u32))?;
    let removed = added.tombstone(added.clone())?;

    node.local_cache_put(removed).await?;
    node.local_cache_put(added).await?;

    let cached = node.local_cache_get(resource).await?;
    assert_eq!(cached.map(|entry| entry.data), Some(Vec::new()));
    Ok(())
}
