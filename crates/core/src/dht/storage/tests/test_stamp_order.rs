//! The issue law of a node's stamps (#1075): a node stamps its operations in issue order, so
//! two operations it issues within one millisecond are ordered, and an append after an
//! overwrite survives that overwrite's reset floor, without its cache (the writer-order tests
//! of `test_own_write` cover the cache, #1033).

use super::super::super::chord::PeerRing;
use crate::consts::TS_OFFSET_TOLERANCE_MS;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::EntryVersion;
use crate::dht::Did;
use crate::dht::StorageKey;
use crate::error::Error;
use crate::error::Result;
use crate::storage::MemStorage;
use crate::tests::data_carrier;
use crate::tests::stamped_delta;
use crate::tests::stamped_operation;
use crate::utils::get_epoch_ms;

/// Every operation a node stamps at one millisecond is stamped strictly after the one before
/// it, whatever its digest.
#[tokio::test]
async fn test_a_nodes_stamps_ascend_within_one_millisecond() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let mut previous = None;
    for value in ["a", "b", "c", "d", "e", "f", "g", "h"] {
        let op = node
            .stamp_operation(
                EntryOperation::Extend(data_carrier(topic, &[value], now_ms)),
                now_ms,
                None,
            )
            .await?;
        let version = op.entry().max_observed_version();
        assert!(
            version > previous,
            "{value} is stamped after the operation before it"
        );
        previous = version;
    }
    Ok(())
}

/// The trace of #1075: an overwrite and an append stamped in the same millisecond, applied in
/// order, keep the appended element, since the append's dot is above the overwrite's floor.
#[tokio::test]
async fn test_an_append_after_an_overwrite_in_one_millisecond_survives() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let overwrite = node
        .stamp_operation(
            EntryOperation::Overwrite(data_carrier(topic, &["fixture"], now_ms)),
            now_ms,
            None,
        )
        .await?;
    let append = node
        .stamp_operation(
            EntryOperation::Extend(data_carrier(topic, &["appended"], now_ms)),
            now_ms,
            None,
        )
        .await?;
    node.operate_storage_entry(now_ms, topic, overwrite, node.did)
        .await?;
    node.operate_storage_entry(now_ms, topic, append, node.did)
        .await?;

    let stored = node
        .live_storage_entry(StorageKey::new(EntryKind::Data, topic), now_ms)
        .await?
        .ok_or_else(|| Error::InvalidMessage("the topic is stored".to_string()))?;
    assert_eq!(
        stored.data.len(),
        2,
        "both writes survive: {:?}",
        stored.data
    );
    Ok(())
}

/// Read order at an owner: an operation this node stamps for a placement it holds is stamped
/// after every version of the replica there, even one another writer issued ahead of its clock.
#[tokio::test]
async fn test_an_owners_stamp_follows_the_replica_it_holds() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let ahead = stamped_delta(
        EntryOperation::Extend,
        topic,
        &["held"],
        now_ms + 50,
        Did::from(9u32),
    )?;
    let held = ahead.max_observed_version();
    node.join_storage_entry(now_ms, topic, ahead, Did::from(9u32))
        .await?;

    let op = node
        .stamp_operation(
            EntryOperation::Extend(data_carrier(topic, &["own"], now_ms)),
            now_ms,
            Some(topic),
        )
        .await?;
    assert!(op.entry().max_observed_version() > held);
    Ok(())
}

/// A burst at the cap (#1075): after the node passes through a version `σ` ahead of its clock,
/// 10 000 stamps in one millisecond still ascend strictly, each below `now + σ`, and every one is
/// admitted by an owner at that clock: the counter, not the millisecond, carries the ascent.
#[tokio::test]
async fn test_ten_thousand_stamps_at_the_cap_ascend_and_are_admitted() -> Result<()> {
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let edge_ms = now_ms + TS_OFFSET_TOLERANCE_MS;
    let ahead = stamped_operation(
        EntryOperation::Extend,
        topic,
        &["edge"],
        edge_ms,
        Did::from(9u32),
    )?;
    node.stamp_operation(ahead, now_ms, None).await?;

    let mut previous = None;
    for index in 0..10_000u32 {
        let value = index.to_string();
        let op = node
            .stamp_operation(
                EntryOperation::Extend(data_carrier(topic, &[value.as_str()], now_ms)),
                now_ms,
                None,
            )
            .await?;
        let version = op.entry().max_observed_version();
        assert!(version > previous, "stamp {index} ascends");
        assert!(version.is_some_and(|version| version.logical_time_ms < edge_ms));
        op.validate_admissible_at(now_ms, node.network_id())?;
        previous = version;
    }
    Ok(())
}

/// The stamp tracks the clock (#1075): once the clock passes the floor, the millisecond moves up
/// to it and the counter restarts, so a dot's issue time `τ`, which retention, stability and
/// leases read, is the clock's.
#[tokio::test]
async fn test_a_stamp_after_the_clock_moves_takes_the_clock_and_restarts_the_counter() -> Result<()>
{
    let node = PeerRing::new_with_storage(Did::from(0u32), 3, Box::new(MemStorage::new()));
    let now_ms = get_epoch_ms();
    let topic = Did::from(10u32);
    let stamp = |value: &'static str, at_ms: u128| {
        node.stamp_operation(
            EntryOperation::Extend(data_carrier(topic, &[value], at_ms)),
            at_ms,
            None,
        )
    };
    stamp("a", now_ms).await?;
    let repeated = stamp("b", now_ms).await?.entry().max_observed_version();
    assert_eq!(repeated.map(|version| version.counter), Some(1));
    let later = stamp("c", now_ms + 5).await?.entry().max_observed_version();
    assert_eq!(
        later.map(|version| (version.logical_time_ms, version.counter)),
        Some((now_ms + 5, 0))
    );
    Ok(())
}

/// The counter is total: a floor at `u32::MAX` within the clock's millisecond refuses the next
/// version instead of wrapping or advancing the millisecond past the floor.
#[test]
fn test_the_counter_refuses_at_its_bound() {
    let now_ms = get_epoch_ms();
    let floor = EntryVersion {
        counter: u32::MAX,
        ..EntryVersion::new(now_ms, Did::from(1u32), Did::from(2u32))
    };
    let issued = EntryOperation::Extend(data_carrier(Did::from(10u32), &["x"], now_ms))
        .stamped_after(now_ms, Did::from(0u32), Some(floor));
    assert!(matches!(issued, Err(Error::EntryVersionAheadOfClock)));
}
