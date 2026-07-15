use std::sync::Arc;

use super::super::ChordStorageInterface;
use super::super::ChordStorageInterfaceCacheChecker;
use super::test_support::next_generated_key;
use super::test_support::next_payload;
use super::test_support::non_affine_placement;
use super::test_support::prepare_node_with_storage_redundancy;
use super::test_support::split_redundant_entry;
use super::test_support::NoopCallback;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Did;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::FoundEntry;
use crate::message::types::Message;
use crate::message::Encoder;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::prelude::entry::EntryKind;
use crate::prelude::entry::EntryOperation;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::transport::STORAGE_LOOKUP_OBSERVATION_CAPACITY;
use crate::swarm::Swarm;
use crate::swarm::SwarmBuilder;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn leave_dht_republishes_after_responsibility_peer_departure() -> Result<()> {
    let key = SecretKey::random();
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0)
        .build(),
    );
    let node = Node::new(swarm);
    let departed = Did::from(100u32);
    node.dht().successors().update(departed)?;
    let entry = Entry::new(key.address().into(), vec![], EntryKind::Data);
    let placement_keys = entry.did.rotate_affine(2)?;
    node.dht()
        .storage
        .put(&placement_keys[0].to_string(), &entry)
        .await?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

    handler.leave_dht(departed).await?;

    assert!(!node.dht().successors().contains(&departed)?);
    assert_eq!(
        node.dht()
            .storage
            .get(&placement_keys[1].to_string())
            .await?,
        Some(entry)
    );
    Ok(())
}

#[tokio::test]
async fn storage_api_rejects_redundancy_mismatch() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;

    let result = <Swarm as ChordStorageInterface<2>>::storage_fetch(&node.swarm, node.did()).await;

    assert!(matches!(
        result,
        Err(Error::StorageRedundancyMismatch {
            configured: 1,
            requested: 2
        })
    ));
    Ok(())
}

#[tokio::test]
async fn placed_entry_operation_rejects_non_affine_placement() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let topic = "reject misplaced remote storage operation".to_string();
    let entry: Entry = topic.try_into()?;
    let invalid_placement = non_affine_placement(entry.did, 2)?;
    let msg = PlacedEntryOperation {
        placement: invalid_placement,
        op: EntryOperation::Overwrite(entry.clone()),
    };
    let sender_session = SessionSk::new_with_seckey(&SecretKey::random())?;
    let context = MessagePayload::new_send(
        Message::OperateEntry(msg.clone()),
        &sender_session,
        node.did(),
        node.did(),
    )?;

    assert!(!msg.placement_belongs_to_entry(2)?);
    let result = handler.handle(&context, &msg).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("affine replica set")
    ));
    assert_eq!(
        node.dht()
            .storage
            .get(&invalid_placement.to_string())
            .await?,
        None
    );
    assert_eq!(node.dht().storage.get(&entry.did.to_string()).await?, None);
    Ok(())
}

#[tokio::test]
async fn remote_redundant_store_writes_split_replica_at_affine_placement() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let node1 = prepare_node_with_storage_redundancy(next_generated_key(&mut keys)?, 2)?;
    let node2 = prepare_node_with_storage_redundancy(next_generated_key(&mut keys)?, 2)?;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;

    let nodes = [&node1, &node2];
    let (entry, primary, replica, primary_owner, replica_owner) = split_redundant_entry(&nodes)?;
    assert_eq!(primary, entry.did);
    let writer = nodes[primary_owner];
    let remote_replica_owner = nodes[replica_owner];

    <Swarm as ChordStorageInterface<2>>::storage_store(&writer.swarm, entry.clone()).await?;

    let payload = next_payload(remote_replica_owner).await?;
    assert!(matches!(
        payload.transaction.data()?,
        Message::OperateEntry(PlacedEntryOperation {
            placement,
            op: EntryOperation::Overwrite(remote_entry),
        }) if placement == replica && remote_entry.did == entry.did
    ));
    assert_eq!(
        writer
            .dht()
            .storage
            .get(&primary.to_string())
            .await?
            .map(|stored| stored.did),
        Some(entry.did)
    );
    assert_eq!(
        remote_replica_owner
            .dht()
            .storage
            .get(&replica.to_string())
            .await?
            .map(|stored| stored.did),
        Some(entry.did)
    );
    assert_eq!(
        remote_replica_owner
            .dht()
            .storage
            .get(&primary.to_string())
            .await?,
        None
    );
    assert_no_more_msg([&node1, &node2]).await;
    Ok(())
}

#[tokio::test]
async fn local_hit_read_repair_sends_no_search_for_unknown_replicas() -> Result<()> {
    let key = SecretKey::random();
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0)
        .build(),
    );
    let node = Node::new(swarm);
    let entry = Entry::new(
        key.address().into(),
        vec!["local".to_string().encode()?],
        EntryKind::Data,
    );
    let first_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .next()
        .ok_or_else(|| Error::InvalidMessage("expected first placement".to_string()))?;
    node.dht()
        .storage
        .put(&first_key.to_string(), &entry)
        .await?;

    <Swarm as ChordStorageInterface<2>>::storage_fetch(&node.swarm, entry.did).await?;

    assert_eq!(node.swarm.storage_check_cache(entry.did).await, Some(entry));
    assert_no_more_msg([&node]).await;
    Ok(())
}

#[tokio::test]
async fn found_entry_repairs_buffered_misses_only() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = Entry::new(
        Did::from(10u32),
        vec!["repair".to_string().encode()?],
        EntryKind::Data,
    );
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let placement_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected repair placement".to_string()))?;
    let unknown_key = Did::from(120u32);
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.start_storage_lookup(entry.did, 2)?;

    handler
        .handle(&context, &FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        })
        .await?;
    handler
        .handle(&context, &FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource: entry.did,
            redundancy: 2,
        })
        .await?;

    assert_eq!(
        node.dht().storage.get(&placement_key.to_string()).await?,
        Some(stored_entry)
    );
    assert_eq!(
        node.dht().storage.get(&unknown_key.to_string()).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn found_entry_rejects_multiple_entries() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let first = Entry::new(
        resource,
        vec!["first".to_string().encode()?],
        EntryKind::Data,
    );
    let second = Entry::new(
        resource,
        vec!["second".to_string().encode()?],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![first.clone(), second.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;

    let result = handler
        .handle(&context, &FoundEntry {
            data: vec![first, second],
            misses: vec![],
            resource,
            redundancy: 2,
        })
        .await;

    assert!(
        matches!(result, Err(Error::InvalidMessage(message)) if message.contains("more than one"))
    );
    assert_eq!(node.swarm.storage_check_cache(resource).await, None);
    assert_eq!(node.swarm.transport.storage_lookup_observation_count()?, 0);
    Ok(())
}

#[tokio::test]
async fn found_entry_rejects_redundancy_outside_local_protocol_mode() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = Entry::new(
        resource,
        vec!["wrong redundancy".to_string().encode()?],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 3,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.start_storage_lookup(resource, 2)?;

    let result = handler
        .handle(&context, &FoundEntry {
            data: vec![entry],
            misses: vec![],
            resource,
            redundancy: 3,
        })
        .await;

    assert!(matches!(
        result,
        Err(Error::StorageRedundancyMismatch {
            configured: 2,
            requested: 3
        })
    ));
    assert_eq!(node.swarm.storage_check_cache(resource).await, None);
    Ok(())
}

#[tokio::test]
async fn found_entry_rejects_response_without_active_lookup() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = Entry::new(
        resource,
        vec!["unsolicited".to_string().encode()?],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;

    let result = handler
        .handle(&context, &FoundEntry {
            data: vec![entry],
            misses: vec![],
            resource,
            redundancy: 2,
        })
        .await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("no active local lookup")
    ));
    assert_eq!(node.swarm.storage_check_cache(resource).await, None);
    Ok(())
}

#[tokio::test]
async fn found_entry_rejects_resource_mismatch_without_cache_write() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = Entry::new(
        Did::from(11u32),
        vec!["wrong resource".to_string().encode()?],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.start_storage_lookup(resource, 2)?;

    let result = handler
        .handle(&context, &FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        })
        .await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("does not match searched resource")
    ));
    assert_eq!(node.swarm.storage_check_cache(entry.did).await, None);
    assert_eq!(node.swarm.storage_check_cache(resource).await, None);
    Ok(())
}

#[tokio::test]
async fn storage_miss_observation_buffer_is_bounded() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    for index in 0..(STORAGE_LOOKUP_OBSERVATION_CAPACITY + 8) {
        let resource = Did::from((index + 1) as u32);
        let placement = Did::from((index + 10_000) as u32);
        node.swarm.transport.start_storage_lookup(resource, 2)?;
        node.swarm
            .transport
            .observe_storage_misses(resource, 2, [PlacementMiss::new(placement, node.did())])?;
    }

    assert!(
        node.swarm.transport.storage_lookup_observation_count()?
            <= STORAGE_LOOKUP_OBSERVATION_CAPACITY
    );
    Ok(())
}

#[tokio::test]
async fn storage_fetch_starts_fresh_observation_round() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let resource = Did::from(10u32);
    let placement = Did::from(100u32);
    node.swarm.transport.start_storage_lookup(resource, 1)?;
    node.swarm
        .transport
        .observe_storage_misses(resource, 1, [PlacementMiss::new(placement, node.did())])?;

    node.swarm.transport.start_storage_lookup(resource, 1)?;
    let misses = node.swarm.transport.take_storage_misses(resource, 1)?;

    assert!(misses.is_empty());
    assert_eq!(node.swarm.transport.storage_lookup_observation_count()?, 1);
    Ok(())
}

#[tokio::test]
async fn expired_storage_response_does_not_update_cache_or_repair() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = Entry::new(
        Did::from(10u32),
        vec!["fresh".to_string().encode()?],
        EntryKind::Data,
    );
    let placement_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected repair placement".to_string()))?;
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        }),
        &context_session,
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.start_storage_lookup(entry.did, 2)?;

    handler
        .handle(&context, &FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        })
        .await?;
    node.swarm
        .transport
        .expire_storage_lookup_observation(entry.did, 2)?;
    let result = handler
        .handle(&context, &FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource: entry.did,
            redundancy: 2,
        })
        .await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("no active local lookup")
    ));
    assert_eq!(node.swarm.storage_check_cache(entry.did).await, None);
    assert_eq!(
        node.dht().storage.get(&placement_key.to_string()).await?,
        None
    );
    Ok(())
}
