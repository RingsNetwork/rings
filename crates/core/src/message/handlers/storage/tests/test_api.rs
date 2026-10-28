use std::cmp::Ordering;

use bytes::Bytes;

use super::test_support::assert_cached_data_values;
use super::test_support::next_generated_key;
use super::test_support::next_payload_matching;
use crate::consts::DEFAULT_TTL_MS;
use crate::consts::MAX_TTL_MS;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::Did;
use crate::ecc::tests::gen_ordered_keys;
use crate::error::Error;
use crate::error::Result;
use crate::lifecycle::StopToken;
use crate::message::types::Message;
use crate::message::ChordStorageInterface;
use crate::message::ChordStorageInterfaceCacheChecker;
use crate::message::MessageVerificationExt;
use crate::swarm::storage_api::StoreQuorum;
use crate::tests::activity::probe_on_activity;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::default::TEST_HANG_GUARD;
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
    let entry: Entry = (data.clone(), data.clone()).try_into()?;
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
                    ..
                }) if placement == entry_key && x.did == entry_key
            ))
    })
    .await?;

    // Read-your-writes (#982): the writer observes its own write before any fetch.
    assert_cached_data_values(&node1, entry_key, &[data.as_str()]).await?;
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    // The tap observes the overwrite before its handler runs (`InboundTap`): wait for the write.
    let node2_storage = &node2.dht().storage;
    probe_on_activity(
        "the remote owner stores the entry",
        TEST_HANG_GUARD,
        || async { Ok((node2_storage.count().await? != 0).then_some(())) },
    )
    .await?;
    assert!(node1.dht().storage.count().await? == 0);

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
    let entry: Entry = (topic.clone(), topic.clone()).try_into()?;
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
        .storage_append_data(&topic, Bytes::from("111"))
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1
        .swarm
        .storage_append_data(&topic, Bytes::from("222"))
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    // Read-your-writes (#982): the writer observes its own appends before any fetch.
    assert_cached_data_values(&node1, entry_key, &["111", "222"]).await?;
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert!(node1.dht().storage.count().await? == 0);
    assert!(node2.dht().storage.count().await? != 0);

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "222"]).await?;

    node1
        .swarm
        .storage_append_data(&topic, Bytes::from("333"))
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
    let entry: Entry = (topic.clone(), topic.clone()).try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222", "333", "222"] {
        node1
            .swarm
            .storage_append_data(&topic, Bytes::from(value.to_string()))
            .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    // Read-your-writes (#982): the writer observes its own appends before any fetch.
    assert_cached_data_values(&node1, entry_key, &["111", "333", "222"]).await?;
    assert!(node2.swarm.storage_check_cache(entry_key).await.is_none());
    assert_eq!(node1.dht().storage.count().await?, 0);
    assert_ne!(node2.dht().storage.count().await?, 0);

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["111", "333", "222"]).await?;

    Ok(())
}

/// Two connected nodes, settled, as `(writer, owner)`: the owner holds the placement of
/// `entry_key`, so the writer's every write to it is remote.
async fn writer_and_remote_owner(entry_key: Did) -> Result<(Node, Node)> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let key1 = next_generated_key(&mut keys)?;
    let key2 = next_generated_key(&mut keys)?;
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    Ok(
        match key_strictly_between(entry_key, node2.did(), node1.did()) {
            true => (node1, node2),
            false => (node2, node1),
        },
    )
}

/// Law (`storage_extend`): one delta carries every element in one message per placement, the
/// owner holds it under the requested bound, and its elements outlive the default lifetime an
/// append would request, retiring only at the element horizon `τ + H`.
#[tokio::test]
async fn test_storage_extend_holds_one_delta_for_its_requested_lifetime() -> Result<()> {
    let topic = "one extend carries every element under the bound its writer asks".to_string();
    let entry_key = Entry::gen_did(&topic)?;
    let (node1, node2) = writer_and_remote_owner(entry_key).await?;

    let elements = vec![Bytes::from("111"), Bytes::from("222")];
    node1
        .swarm
        .storage_extend(&topic, elements.clone(), MAX_TTL_MS)
        .await?;
    next_payload_matching(&node2, "one remote extend of both elements", |payload| {
        Ok(matches!(
            payload.transaction.data()?,
            Message::OperateEntry(PlacedEntryOperation {
                placement,
                op: EntryOperation::Extend(x),
                ..
            }) if placement == entry_key && x.data == elements
        ))
    })
    .await?;
    // Read-your-writes (#982): the writer observes its own delta before any fetch.
    assert_cached_data_values(&node1, entry_key, &["111", "222"]).await?;

    let owner_storage = &node2.dht().storage;
    let stored = probe_on_activity("the remote owner stores the delta", TEST_HANG_GUARD, || {
        let elements = &elements;
        async move {
            let stored = owner_storage.get(&entry_key.to_string()).await?;
            Ok(stored.filter(|stored| stored.data == *elements))
        }
    })
    .await?;
    let versions = stored
        .crdt
        .dots
        .iter()
        .map(|dot| dot.version)
        .collect::<Vec<_>>();
    let [first, second] = versions.as_slice() else {
        panic!("the stored delta has one dot per element: {versions:?}");
    };
    assert_eq!(first, second, "one delta stamps one version");
    let issued_at = first.logical_time_ms;
    let bound = stored
        .expires_at_ms
        .expect("a stored entry carries its bound");
    assert!(
        bound >= issued_at + u128::from(MAX_TTL_MS) - crate::consts::TS_OFFSET_TOLERANCE_MS,
        "the owner keeps the requested bound"
    );
    assert_eq!(
        stored
            .clone()
            .retired_at(issued_at + u128::from(DEFAULT_TTL_MS))
            .data,
        elements,
        "the elements outlive the default lifetime an append requests"
    );
    assert!(
        stored
            .retired_at(issued_at + u128::from(MAX_TTL_MS))
            .data
            .is_empty(),
        "the elements retire at the element horizon"
    );
    Ok(())
}

/// Law (`storage_extend_reported`, #866): under [`StoreQuorum::Any`], the receipt of a delta
/// whose placement is remote resolves stored at its owner's report, which the owner sends once
/// it holds the delta.
#[tokio::test]
async fn test_storage_extend_reported_resolves_at_its_owners_report() -> Result<()> {
    let topic = "a reported extend resolves at its owner's report".to_string();
    let entry_key = Entry::gen_did(&topic)?;
    let (writer, owner) = writer_and_remote_owner(entry_key).await?;

    let elements = vec![Bytes::from("111"), Bytes::from("222")];
    let receipt = writer
        .swarm
        .scoped_storage(StopToken::never())
        .storage_extend_reported(&topic, elements.clone(), MAX_TTL_MS, StoreQuorum::Any)
        .await?;
    let stored = tokio::time::timeout(TEST_HANG_GUARD, receipt)
        .await
        .map_err(|_| Error::InvalidMessage("no store report within the hang guard".to_string()))?;

    assert_eq!(stored, Ok(()));
    let held = owner.dht().storage.get(&entry_key.to_string()).await?;
    assert_eq!(held.map(|held| held.data), Some(elements));
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
    let entry: Entry = (topic.clone(), topic.clone()).try_into()?;
    let entry_key = entry.did;

    let (node1, node2) = if key_strictly_between(entry_key, node2.did(), node1.did()) {
        (node1, node2)
    } else {
        (node2, node1)
    };

    for value in ["111", "222"] {
        node1
            .swarm
            .storage_append_data(&topic, Bytes::from(value.to_string()))
            .await?;
        wait_for_msgs([&node1, &node2]).await;
        assert_no_more_msg([&node1, &node2]).await;
    }

    node1
        .swarm
        .storage_tombstone_data(&topic, Bytes::from("111"))
        .await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    node1.swarm.storage_fetch(entry_key).await?;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    assert_cached_data_values(&node1, entry_key, &["222"]).await?;

    Ok(())
}
