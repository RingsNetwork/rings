use std::sync::Arc;

use super::test_support::next_payload_for_tx;
use super::test_support::storage_sync_report_payload;
use super::test_support::NoopCallback;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::entry::EntryKind;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn sync_entries_report_handler_deletes_only_acked_keys() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let acked_key = Did::from(100u32);
    let pending_key = Did::from(120u32);
    let acked_entry = Entry::new(Did::from(10u32), vec![], EntryKind::Data);
    let acked_storage_entry = acked_entry.clone().try_into_storage_entry()?;
    let pending_entry = Entry::new(Did::from(20u32), vec![], EntryKind::Data);
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(node.did()),
        data: vec![PlacedEntry::new(acked_key, acked_entry.clone())],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        node.swarm.transport.session_sk(),
        node.did(),
        node.did(),
    )?;
    node.swarm.transport.record_pending_storage_sync_ack(
        request.transaction.tx_id,
        sync_msg.purpose,
        sync_msg.destination,
        node.did(),
        &sync_msg.data,
    )?;
    node.dht()
        .storage
        .put(&acked_key.to_string(), &acked_entry)
        .await?;
    node.dht()
        .storage
        .put(&pending_key.to_string(), &pending_entry)
        .await?;

    let report = SyncEntriesWithSuccessorReport::new(
        sync_msg.purpose,
        sync_msg.destination,
        node.did(),
        vec![SyncedEntryAck::new(acked_key, acked_storage_entry)],
    );
    let context = storage_sync_report_payload(
        &request,
        report.clone(),
        node.swarm.transport.session_sk(),
        node.did(),
        node.did(),
    )?;

    handler.handle(&context, &report).await?;

    assert_eq!(node.dht().storage.get(&acked_key.to_string()).await?, None);
    assert_eq!(
        node.dht().storage.get(&pending_key.to_string()).await?,
        Some(pending_entry)
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_report_handler_rejects_untracked_acks() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let acked_key = Did::from(100u32);
    let acked_entry = Entry::new(Did::from(10u32), vec![], EntryKind::Data);
    let acked_storage_entry = acked_entry.clone().try_into_storage_entry()?;
    node.dht()
        .storage
        .put(&acked_key.to_string(), &acked_entry)
        .await?;

    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(node.did()),
        data: vec![PlacedEntry::new(acked_key, acked_entry.clone())],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        node.swarm.transport.session_sk(),
        node.did(),
        node.did(),
    )?;
    let report = SyncEntriesWithSuccessorReport::new(
        sync_msg.purpose,
        sync_msg.destination,
        node.did(),
        vec![SyncedEntryAck::new(acked_key, acked_storage_entry)],
    );
    let context = storage_sync_report_payload(
        &request,
        report.clone(),
        node.swarm.transport.session_sk(),
        node.did(),
        node.did(),
    )?;

    let result = handler.handle(&context, &report).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("no pending capability")
    ));
    assert_eq!(
        node.dht().storage.get(&acked_key.to_string()).await?,
        Some(acked_entry)
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_report_handler_forwards_before_pending_capability_check() -> Result<()> {
    let sender = prepare_node(SecretKey::random()).await;
    let relay = prepare_node(SecretKey::random()).await;
    let receiver = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&sender.swarm, &relay.swarm).await;
    wait_for_msgs([&sender, &relay]).await;
    assert_no_more_msg([&sender, &relay]).await;

    let handler = MessageHandler::new(relay.swarm.transport.clone(), Arc::new(NoopCallback));
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        receiver.did(),
    )?;
    let report = SyncEntriesWithSuccessorReport::new(
        sync_msg.purpose,
        sync_msg.destination,
        receiver.did(),
        vec![],
    );
    let context = storage_sync_report_payload(
        &request,
        report.clone(),
        receiver.swarm.transport.session_sk(),
        relay.did(),
        sender.did(),
    )?;

    handler.handle(&context, &report).await?;

    let forwarded = next_payload_for_tx(&sender, request.transaction.tx_id).await?;
    match forwarded.transaction.data::<Message>()? {
        Message::SyncEntriesWithSuccessorReport(forwarded_report) => {
            assert_eq!(forwarded_report.purpose, report.purpose);
            assert_eq!(forwarded_report.destination, report.destination);
            assert_eq!(forwarded_report.receiver, report.receiver);
            assert_eq!(forwarded_report.acks, report.acks);
        }
        message => {
            return Err(Error::InvalidMessage(format!(
                "expected SyncEntriesWithSuccessorReport, got {message:?}"
            )))
        }
    }
    Ok(())
}

#[tokio::test]
async fn additive_repair_sync_cannot_create_pending_cleanup_capability() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let placement_key = Did::from(100u32);
    let entry = Entry::new(Did::from(100u32), vec![], EntryKind::Data);
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(node.did()),
        data: vec![PlacedEntry::new(placement_key, entry)],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        node.swarm.transport.session_sk(),
        node.did(),
        node.did(),
    )?;

    let result = node.swarm.transport.record_pending_storage_sync_ack(
        request.transaction.tx_id,
        sync_msg.purpose,
        sync_msg.destination,
        node.did(),
        &sync_msg.data,
    );

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message))
            if message.contains("does not permit pending cleanup ack")
    ));
    Ok(())
}

#[tokio::test]
async fn sync_entries_report_handler_rejects_wrong_physical_receiver() -> Result<()> {
    let sender = prepare_node(SecretKey::random()).await;
    let receiver = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(sender.swarm.transport.clone(), Arc::new(NoopCallback));
    let acked_key = Did::from(100u32);
    let acked_entry = Entry::new(Did::from(10u32), vec![], EntryKind::Data);
    let acked_storage_entry = acked_entry.clone().try_into_storage_entry()?;
    sender
        .dht()
        .storage
        .put(&acked_key.to_string(), &acked_entry)
        .await?;

    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(acked_key, acked_entry.clone())],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        receiver.did(),
    )?;
    sender.swarm.transport.record_pending_storage_sync_ack(
        request.transaction.tx_id,
        sync_msg.purpose,
        sync_msg.destination,
        receiver.did(),
        &sync_msg.data,
    )?;
    let report = SyncEntriesWithSuccessorReport::new(
        sync_msg.purpose,
        sync_msg.destination,
        sender.did(),
        vec![SyncedEntryAck::new(acked_key, acked_storage_entry)],
    );
    let context = storage_sync_report_payload(
        &request,
        report.clone(),
        sender.swarm.transport.session_sk(),
        sender.did(),
        sender.did(),
    )?;

    let result = handler.handle(&context, &report).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("receiver does not match pending sync")
    ));
    assert_eq!(
        sender.dht().storage.get(&acked_key.to_string()).await?,
        Some(acked_entry)
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_report_handler_rejects_unproven_placement_receiver() -> Result<()> {
    let sender = prepare_node(SecretKey::random()).await;
    let route_next_hop = prepare_node(SecretKey::random()).await;
    let final_receiver = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(sender.swarm.transport.clone(), Arc::new(NoopCallback));
    let placement_key = Did::from(100u32);
    let acked_entry = Entry::new(Did::from(10u32), vec![], EntryKind::Data);
    let acked_storage_entry = acked_entry.clone().try_into_storage_entry()?;
    sender
        .dht()
        .storage
        .put(&placement_key.to_string(), &acked_entry)
        .await?;

    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PlacementKey(placement_key),
        data: vec![PlacedEntry::new(placement_key, acked_entry.clone())],
    };
    let request = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        route_next_hop.did(),
        sync_msg.destination.did(),
    )?;
    sender.swarm.transport.record_pending_storage_sync_ack(
        request.transaction.tx_id,
        sync_msg.purpose,
        sync_msg.destination,
        route_next_hop.did(),
        &sync_msg.data,
    )?;
    let report = SyncEntriesWithSuccessorReport::new(
        sync_msg.purpose,
        sync_msg.destination,
        final_receiver.did(),
        vec![SyncedEntryAck::new(placement_key, acked_storage_entry)],
    );
    let context = storage_sync_report_payload(
        &request,
        report.clone(),
        final_receiver.swarm.transport.session_sk(),
        sender.did(),
        sender.did(),
    )?;

    let result = handler.handle(&context, &report).await;

    assert!(matches!(
        result,
        Err(Error::InvalidMessage(message)) if message.contains("receiver does not match pending sync")
    ));
    assert_eq!(
        sender.dht().storage.get(&placement_key.to_string()).await?,
        Some(acked_entry)
    );
    Ok(())
}
