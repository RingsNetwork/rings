use std::sync::Arc;

use bytes::Bytes;

use super::super::ChordStorageInterface;
use super::super::ChordStorageInterfaceCacheChecker;
#[cfg(feature = "dummy")]
use super::test_support::install_two_node_chord_view;
use super::test_support::next_generated_key;
use super::test_support::next_payload_matching;
use super::test_support::non_affine_placement;
use super::test_support::prepare_node_with_storage_redundancy;
use super::test_support::split_redundant_entry;
use super::test_support::NoopCallback;
use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryOperation;
use crate::dht::entry::EntryTombstone;
use crate::dht::entry::EntryVersion;
use crate::dht::entry::PlacedEntryOperation;
use crate::dht::entry::PlacementMiss;
use crate::dht::Did;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::FoundEntry;
use crate::message::types::Message;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::MessageVerificationExt;
use crate::storage::MemStorage;
use crate::swarm::transport::STORAGE_LOOKUP_OBSERVATION_CAPACITY;
use crate::swarm::SwarmBuilder;
use crate::tests::default::assert_no_more_msg;
#[cfg(feature = "dummy")]
use crate::tests::default::dummy_hooks::PendingSendGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

#[tokio::test]
async fn test_storage_repair_request_after_claim_remains_pending() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;

    assert!(!node.swarm.transport.storage_repair_requested());
    node.swarm.transport.request_storage_repair();
    assert!(node.swarm.transport.claim_storage_repair());
    assert!(!node.swarm.transport.storage_repair_requested());

    node.swarm.transport.request_storage_repair();
    assert!(node.swarm.transport.storage_repair_requested());
    Ok(())
}

#[tokio::test]
async fn test_leave_dht_defers_repair_until_maintenance_runs() -> Result<()> {
    let key = SecretKey::random();
    let session = DelegateeKey::new_with_seckey(&key)?;
    let node = Node::build(
        SwarmBuilder::new(
            0,
            crate::tests::default::TEST_ICE_SERVERS,
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0),
    );
    let departed = Did::from(100u32);
    node.dht().successors().update(departed)?;
    let entry = live_entry(key.address().into(), vec![], EntryKind::Data);
    let placement_keys = entry.did.rotate_affine(2)?;
    node.dht()
        .storage
        .put(&placement_keys[0].to_string(), &entry)
        .await?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

    handler.leave_dht(departed).await?;

    assert!(!node.dht().successors().contains(&departed)?);
    assert!(node.swarm.transport.storage_repair_requested());
    assert_eq!(
        node.dht()
            .storage
            .get(&placement_keys[1].to_string())
            .await?,
        None
    );

    node.swarm.stabilizer().stabilize().await?;

    assert!(!node.swarm.transport.storage_repair_requested());
    assert_eq!(
        node.dht()
            .storage
            .get(&placement_keys[1].to_string())
            .await?,
        Some(entry)
    );
    Ok(())
}

/// Disconnecting an admitted peer that no topology slot references leaves the placement
/// view unchanged, so no repair round is requested; the departure of an admitted successor
/// vacates a slot and requests one.
#[tokio::test]
async fn test_leave_dht_attempt_requests_repair_only_for_a_referenced_peer() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let transport = node.swarm.transport.clone();
    let handler = MessageHandler::new(transport.clone(), Arc::new(NoopCallback));

    let unreferenced = SecretKey::random().address().into();
    let attempt = transport
        .reserve_pending_connection_with_observer_for_test(unreferenced, || {}, || {})
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    handler.leave_dht_attempt(attempt).await?;
    assert!(!transport.is_active_connection_attempt(attempt));
    assert!(!transport.storage_repair_requested());

    let referenced = SecretKey::random().address().into();
    let attempt = transport
        .reserve_pending_connection_with_observer_for_test(referenced, || {}, || {})
        .await?;
    assert!(transport.activate_connection_for_test(attempt)?);
    transport.dht.admit_connected(referenced, None)?;
    assert!(transport.dht.successors().contains(&referenced)?);
    handler.leave_dht_attempt(attempt).await?;
    assert!(!transport.dht.successors().contains(&referenced)?);
    assert!(transport.storage_repair_requested());
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_found_entry_read_repair_backpressure_is_deferred() -> Result<()> {
    let node1 = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let node2 = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    install_two_node_chord_view(&node1, &node2)?;
    wait_for_msgs([&node1, &node2]).await;

    let (entry, primary, replica, primary_owner, replica_owner) =
        split_redundant_entry(&[&node1, &node2])?;
    let remote_placement = if primary_owner == 1 {
        primary
    } else if replica_owner == 1 {
        replica
    } else {
        return Err(Error::InvalidMessage(
            "expected a replica owned by node2".to_string(),
        ));
    };
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource: entry.did,
            redundancy: 2,
        }),
        node2.swarm.transport.message_signer(),
        node1.did(),
        node1.did(),
    )?;
    let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
    node1.swarm.transport.start_storage_lookup(entry.did, 2)?;

    handler
        .handle(&context, &FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(remote_placement, node2.did())],
            resource: entry.did,
            redundancy: 2,
        })
        .await?;

    let _pending_send = PendingSendGuard::new();
    handler
        .handle(&context, &FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource: entry.did,
            redundancy: 2,
        })
        .await?;

    assert_eq!(
        node1.swarm.storage_check_cache(entry.did).await,
        Some(entry)
    );
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );
    assert!(node1.swarm.transport.get_connection(node2.did()).is_some());
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert_no_more_msg([&node2]).await;
    Ok(())
}

#[tokio::test]
async fn test_placed_entry_operation_rejects_non_affine_placement() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let topic = "reject misplaced remote storage operation".to_string();
    let entry: Entry = crate::tests::live((topic.clone(), topic).try_into()?);
    let invalid_placement = non_affine_placement(entry.did, 2)?;
    let msg = PlacedEntryOperation {
        placement: invalid_placement,
        op: EntryOperation::Overwrite(entry.clone()),
    };
    let sender_session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let context = MessagePayload::new_send(
        Message::OperateEntry(msg.clone()),
        MessageSigner::new(&sender_session, TEST_NETWORK_ID),
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
async fn test_remote_redundant_store_writes_split_replica_at_affine_placement() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
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

    writer.swarm.storage_store(entry.clone()).await?;

    next_payload_matching(
        remote_replica_owner,
        "remote redundant overwrite operation",
        |payload| {
            Ok(payload.transaction.signer() == writer.did()
                && payload.transaction.destination == remote_replica_owner.did()
                && payload.relay.destination == remote_replica_owner.did()
                && matches!(
                    payload.transaction.data()?,
                    Message::OperateEntry(PlacedEntryOperation {
                        placement,
                        op: EntryOperation::Overwrite(remote_entry),
                    }) if placement == replica && remote_entry.did == entry.did
                ))
        },
    )
    .await?;
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
async fn test_local_hit_read_repair_sends_no_search_for_unknown_replicas() -> Result<()> {
    let key = SecretKey::random();
    let session = DelegateeKey::new_with_seckey(&key)?;
    let node = Node::build(
        SwarmBuilder::new(
            0,
            crate::tests::default::TEST_ICE_SERVERS,
            Box::new(MemStorage::new()),
            session,
        )
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0),
    );
    let entry = live_entry(
        key.address().into(),
        vec![Bytes::from("local")],
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

    node.swarm.storage_fetch(entry.did).await?;

    assert_eq!(node.swarm.storage_check_cache(entry.did).await, Some(entry));
    assert_no_more_msg([&node]).await;
    Ok(())
}

#[tokio::test]
async fn test_found_entry_repairs_buffered_misses_only() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("repair")],
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
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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

/// The handler split: a found-empty reply that reports misses repairs them from
/// the carrier the cache holds (`PeerRing::local_cache_held`), even one past its retention
/// bound that the cache serves as absent, since its removes are what the repair spreads. A
/// repair that read the served view (`local_cache_get`) would find nothing and write nothing.
#[tokio::test]
async fn test_found_empty_reply_repairs_misses_from_a_held_carrier_past_its_bound() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let now_ms = get_epoch_ms();
    let resource = Did::from(10u32);
    let mut carrier = Entry::new(resource, vec![], EntryKind::Data);
    carrier.crdt.tombstones = vec![EntryTombstone::of(&Bytes::from("removed"), EntryDot {
        version: EntryVersion::new(now_ms - 1_000, Did::from(1u32), Did::from(2u32)),
        index: 0,
    })];
    carrier.expires_at_ms = Some(now_ms - 1);
    node.dht()
        .cache
        .put(&resource.to_string(), &carrier)
        .await?;
    let placement_key = resource
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected repair placement".to_string()))?;
    let found_empty = FoundEntry {
        data: vec![],
        misses: vec![PlacementMiss::new(placement_key, node.did())],
        resource,
        redundancy: 2,
    };
    let context_session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(found_empty.clone()),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.start_storage_lookup(resource, 2)?;

    assert_eq!(node.swarm.storage_check_cache(resource).await, None);
    handler.handle(&context, &found_empty).await?;

    let repaired = node
        .dht()
        .storage
        .get(&placement_key.to_string())
        .await?
        .ok_or_else(|| Error::InvalidMessage("the missed placement is repaired".to_string()))?;
    assert!(repaired.data.is_empty());
    assert_eq!(repaired.crdt.tombstones, carrier.crdt.tombstones);
    Ok(())
}

#[tokio::test]
async fn test_found_entry_rejects_multiple_entries() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let first = live_entry(resource, vec![Bytes::from("first")], EntryKind::Data);
    let second = live_entry(resource, vec![Bytes::from("second")], EntryKind::Data);
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![first.clone(), second.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_found_entry_rejects_redundancy_outside_local_protocol_mode() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = live_entry(
        resource,
        vec![Bytes::from("wrong redundancy")],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 3,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_found_entry_rejects_response_without_active_lookup() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = live_entry(resource, vec![Bytes::from("unsolicited")], EntryKind::Data);
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_found_entry_rejects_resource_mismatch_without_cache_write() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource = Did::from(10u32);
    let entry = live_entry(
        Did::from(11u32),
        vec![Bytes::from("wrong resource")],
        EntryKind::Data,
    );
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![entry.clone()],
            misses: vec![],
            resource,
            redundancy: 2,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_storage_miss_observation_buffer_is_bounded() -> Result<()> {
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
async fn test_storage_fetch_starts_fresh_observation_round() -> Result<()> {
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

/// A fetch's reply marker (#913 R8): a reply that caches an entry answers the key since every
/// mark read before it, a found-empty one does not, and neither a new round of the key nor the
/// eviction of its bucket makes an answer read as earlier than a mark. A reader polls the marker
/// instead of comparing cached values, which change with no reply as elements cross their
/// horizon.
#[tokio::test]
async fn test_storage_fetch_answers_are_stamped_after_every_earlier_mark() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let redundancy = node.swarm.storage_redundancy();
    let entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("answer")],
        EntryKind::Data,
    );
    let reply = |data: Vec<Entry>| FoundEntry {
        data,
        misses: vec![],
        resource: entry.did,
        redundancy,
    };
    let context_session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(reply(vec![])),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
        node.did(),
        node.did(),
    )?;

    let answered_since = |mark| node.swarm.storage_fetch_answered_since(entry.did, mark);
    let first = node.swarm.storage_fetch_mark();
    node.swarm
        .transport
        .start_storage_lookup(entry.did, redundancy)?;
    assert!(!answered_since(first)?);
    handler.handle(&context, &reply(vec![])).await?;
    assert!(
        !answered_since(first)?,
        "a found-empty reply answers nothing"
    );
    handler
        .handle(&context, &reply(vec![entry.clone()]))
        .await?;
    assert!(answered_since(first)?);

    let second = node.swarm.storage_fetch_mark();
    assert!(
        !answered_since(second)?,
        "an answer before a mark is earlier"
    );
    node.swarm
        .transport
        .start_storage_lookup(entry.did, redundancy)?;
    assert!(
        answered_since(first)?,
        "a concurrent round does not undo what an earlier fetcher saw"
    );

    // A bucket evicted and started again stamps its next answer above every earlier mark.
    node.swarm
        .transport
        .expire_storage_lookup_observation(entry.did, redundancy)?;
    node.swarm
        .transport
        .start_storage_lookup(entry.did, redundancy)?;
    assert!(!answered_since(second)?);
    handler
        .handle(&context, &reply(vec![entry.clone()]))
        .await?;
    assert!(answered_since(second)?);
    Ok(())
}

/// A reply the cache cannot serve is not an answer: a carrier one placement read just before
/// its retention bound, received just after it (or under a clock up to σ ahead), is admitted
/// and cached, since an unstable remove holds it live, but it is served as absent, so the
/// marker stays put; the live reply a second placement sends afterwards is the answer.
#[tokio::test]
async fn test_a_reply_past_its_bound_is_no_answer_and_a_later_live_placement_is() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let redundancy = node.swarm.storage_redundancy();
    let now_ms = get_epoch_ms();
    let live = live_entry(
        Did::from(10u32),
        vec![Bytes::from("answer")],
        EntryKind::Data,
    );
    let mut past_bound = live.clone();
    past_bound.expires_at_ms = Some(now_ms - 1);
    past_bound.crdt.tombstones = vec![EntryTombstone::of(&Bytes::from("removed"), EntryDot {
        version: EntryVersion::new(now_ms - 1_000, Did::from(1u32), Did::from(2u32)),
        index: 0,
    })];
    let reply = |data: Vec<Entry>| FoundEntry {
        data,
        misses: vec![],
        resource: live.did,
        redundancy,
    };
    let context_session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(reply(vec![])),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
        node.did(),
        node.did(),
    )?;

    let mark = node.swarm.storage_fetch_mark();
    node.swarm
        .transport
        .start_storage_lookup(live.did, redundancy)?;
    handler.handle(&context, &reply(vec![past_bound])).await?;
    assert!(!node.swarm.storage_fetch_answered_since(live.did, mark)?);
    assert_eq!(node.swarm.storage_check_cache(live.did).await, None);

    handler.handle(&context, &reply(vec![live.clone()])).await?;
    assert!(node.swarm.storage_fetch_answered_since(live.did, mark)?);
    assert!(node.swarm.storage_check_cache(live.did).await.is_some());
    Ok(())
}

#[tokio::test]
async fn test_expired_storage_response_does_not_update_cache_or_repair() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("fresh")],
        EntryKind::Data,
    );
    let placement_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected repair placement".to_string()))?;
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::FoundEntry(FoundEntry {
            data: vec![],
            misses: vec![PlacementMiss::new(placement_key, node.did())],
            resource: entry.did,
            redundancy: 2,
        }),
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
#[cfg(feature = "dummy")]
use crate::message::PayloadSender;
