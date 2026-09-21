use std::cmp::Ordering;

use super::super::ChordStorageInterface;
use super::super::ChordStorageInterfaceCacheChecker;
use super::test_support::assert_cached_data_values;
use super::test_support::next_generated_key;
use super::test_support::next_payload_matching;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::Did;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::Encoder;
use crate::message::MessageVerificationExt;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

/// Whether `key` lies in the open clockwise interval `(lower, upper)`.
fn key_strictly_between(key: Did, lower: Did, upper: Did) -> bool {
    key != lower && Did::cmp_from_observer(lower, key, upper) == Ordering::Less
}

#[tokio::test]
async fn test_storage_store_fetches_remote_entry_into_cache() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let data = "Across the Great Wall we can reach every corner in the world.".to_string();
    let entry: Entry = data.clone().try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    assert_eq!(node1.dht().cache.count().await?, 0);
    assert_eq!(node2.dht().cache.count().await?, 0);
    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

    node1.swarm.storage_store(entry.clone()).await?;
    next_payload_matching(&node2, "remote overwrite operation", |payload| {
        Ok(payload.transaction.signer() == node1.did()
            && payload.transaction.destination == node2.did()
            && payload.relay.destination == node2.did()
            && matches!(
                payload.transaction.data()?,
                Message::OperateEntry(PlacedEntryOperation {
                    placement,
                    op: EntryOperation::Overwrite(x),
                }) if placement == entry_key && x.did == entry_key
            ))
    })
    .await?;

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node1.dht().storage.count().await? == 0);
    assert!(node2.dht().storage.count().await? != 0);

    node1.swarm.storage_fetch(entry_key).await?;

    next_payload_matching(&node2, "remote entry search", |payload| {
        Ok(payload.transaction.signer() == node1.did()
            && payload.transaction.destination == node2.did()
            && payload.relay.destination == node2.did()
            && matches!(
                payload.transaction.data()?,
                Message::SearchEntry(x) if x.resource == entry_key && x.placement == entry_key
            ))
    })
    .await?;

    next_payload_matching(&node1, "entry lookup response", |payload| {
        Ok(payload.transaction.signer() == node2.did()
            && payload.transaction.destination == node1.did()
            && payload.relay.destination == node1.did()
            && matches!(
                payload.transaction.data()?,
                Message::FoundEntry(x)
                    if x.resource == entry_key
                        && x.misses.is_empty()
                        && x.data.first().is_some_and(|entry| entry.did == entry_key)
            ))
    })
    .await?;

    assert_cached_data_values(&node1, entry_key, &[data.as_str()]).await?;

    Ok(())
}

#[tokio::test]
async fn test_storage_append_data_preserves_entry_payload_order() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let topic = "Across the Great Wall we can reach every corner in the world.".to_string();
    let entry: Entry = topic.clone().try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    assert_eq!(node1.dht().cache.count().await?, 0);
    assert_eq!(node2.dht().cache.count().await?, 0);
    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

    node1
        .swarm
        .storage_append_data(&topic, "111".to_string().encode()?)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1
        .swarm
        .storage_append_data(&topic, "222".to_string().encode()?)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node1.dht().storage.count().await? == 0);
    assert!(node2.dht().storage.count().await? != 0);

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "222"]).await?;

    node1
        .swarm
        .storage_append_data(&topic, "333".to_string().encode()?)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "222", "333"]).await?;

    Ok(())
}

#[tokio::test]
async fn test_storage_append_data_moves_existing_entry_payload_to_end_once() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let topic = "touch keeps unique entry payloads ordered by recency".to_string();
    let entry: Entry = topic.clone().try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222", "333", "222"] {
        node1
            .swarm
            .storage_append_data(&topic, value.to_string().encode()?)
            .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert_eq!(node1.dht().storage.count().await?, 0);
    assert_ne!(node2.dht().storage.count().await?, 0);

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "333", "222"]).await?;

    Ok(())
}

#[tokio::test]
async fn test_storage_tombstone_data_removes_observed_payload() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let topic = "tombstone removes stale data topic payloads".to_string();
    let entry: Entry = topic.clone().try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222"] {
        node1
            .swarm
            .storage_append_data(&topic, value.to_string().encode()?)
            .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    node1
        .swarm
        .storage_tombstone_data(&topic, "111".to_string().encode()?)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["222"]).await?;

    Ok(())
}

#[tokio::test]
async fn test_storage_compact_data_prunes_tombstones_and_preserves_owner_values() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let topic = "compact data prunes tombstone metadata".to_string();
    let entry: Entry = topic.clone().try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222", "333"] {
        node1
            .swarm
            .storage_append_data(&topic, value.to_string().encode()?)
            .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    node1
        .swarm
        .storage_tombstone_data(&topic, "111".to_string().encode()?)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let compact_removals = vec!["111".to_string().encode()?];
    node1
        .swarm
        .storage_compact_data(&topic, compact_removals)
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["222", "333"]).await?;
    let entry = node1
        .swarm
        .storage_check_cache(entry_key)
        .await
        .ok_or_else(|| crate::error::Error::InvalidMessage("expected cached entry".to_string()))?;
    assert!(entry.crdt.register.is_some());
    assert!(entry.crdt.tombstones.is_empty());

    Ok(())
}
