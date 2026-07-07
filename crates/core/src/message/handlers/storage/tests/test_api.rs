use super::super::ChordStorageInterface;
use super::super::ChordStorageInterfaceCacheChecker;
use super::test_support::assert_cached_data_values;
use super::test_support::next_generated_key;
use super::test_support::next_payload;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntryOperation;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::Encoder;
use crate::prelude::entry::EntryOperation;
use crate::swarm::Swarm;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn storage_store_fetches_remote_entry_into_cache() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
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

    let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    assert_eq!(node1.dht().cache.count().await?, 0);
    assert_eq!(node2.dht().cache.count().await?, 0);
    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

    <Swarm as ChordStorageInterface<1>>::storage_store(&node1.swarm, entry.clone()).await?;
    let ev = next_payload(&node2).await?;
    assert!(matches!(
        ev.transaction.data()?,
        Message::OperateEntry(PlacedEntryOperation {
            placement,
            op: EntryOperation::Overwrite(x),
        }) if placement == entry_key && x.did == entry_key
    ));

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node1.dht().storage.count().await? == 0);
    assert!(node2.dht().storage.count().await? != 0);

    <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;

    let ev = next_payload(&node2).await?;
    assert!(matches!(
        ev.transaction.data()?,
        Message::SearchEntry(x) if x.resource == entry_key && x.placement == entry_key
    ));

    let ev = next_payload(&node1).await?;
    assert!(matches!(
        ev.transaction.data()?,
        Message::FoundEntry(x)
            if x.resource == entry_key
                && x.misses.is_empty()
                && x.data.first().is_some_and(|entry| entry.did == entry_key)
    ));

    assert_cached_data_values(&node1, entry_key, &[data.as_str()]).await?;

    Ok(())
}

#[tokio::test]
async fn storage_append_data_preserves_entry_payload_order() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
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

    let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    assert_eq!(node1.dht().cache.count().await?, 0);
    assert_eq!(node2.dht().cache.count().await?, 0);
    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());

    <Swarm as ChordStorageInterface<1>>::storage_append_data(
        &node1.swarm,
        &topic,
        "111".to_string().encode()?,
    )
    .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    <Swarm as ChordStorageInterface<1>>::storage_append_data(
        &node1.swarm,
        &topic,
        "222".to_string().encode()?,
    )
    .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node1.dht().storage.count().await? == 0);
    assert!(node2.dht().storage.count().await? != 0);

    <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "222"]).await?;

    <Swarm as ChordStorageInterface<1>>::storage_append_data(
        &node1.swarm,
        &topic,
        "333".to_string().encode()?,
    )
    .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "222", "333"]).await?;

    Ok(())
}

#[tokio::test]
async fn storage_touch_data_moves_existing_entry_payload_to_end_once() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
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

    let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222", "333", "222"] {
        <Swarm as ChordStorageInterface<1>>::storage_touch_data(
            &node1.swarm,
            &topic,
            value.to_string().encode()?,
        )
        .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    assert!(node1.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert_eq!(node1.dht().storage.count().await?, 0);
    assert_ne!(node2.dht().storage.count().await?, 0);

    <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "333", "222"]).await?;

    Ok(())
}

#[tokio::test]
async fn storage_tombstone_data_removes_observed_payload() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
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

    let (node1, node2) = if entry_key.in_range(node2.did(), node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222"] {
        <Swarm as ChordStorageInterface<1>>::storage_touch_data(
            &node1.swarm,
            &topic,
            value.to_string().encode()?,
        )
        .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    <Swarm as ChordStorageInterface<1>>::storage_tombstone_data(
        &node1.swarm,
        &topic,
        "111".to_string().encode()?,
    )
    .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    <Swarm as ChordStorageInterface<1>>::storage_fetch(&node1.swarm, entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["222"]).await?;

    Ok(())
}
