use std::sync::Arc;

use super::super::next_hop_for_sync_entries;
use super::super::persist_synced_entries;
use super::test_support::next_generated_key;
use super::test_support::next_payload;
use super::test_support::next_payload_for_tx;
use super::test_support::physical_sync_route_next_hop;
use super::test_support::prepare_node_with_virtual_nodes;
use super::test_support::storage_sync_route_next_hop;
use super::test_support::NoopCallback;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::Encoder;
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
async fn sync_entries_handler_reports_persisted_entries() -> Result<()> {
    let sender = prepare_node(SecretKey::random()).await;
    let receiver = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    for successor in receiver.dht().successors().list()? {
        receiver.dht().successors().remove(successor)?;
    }
    *receiver.dht().lock_predecessor()? = None;

    let receiver_handler =
        MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = Entry::new(
        Did::from(10u32),
        vec!["handler acked".to_string().encode()?],
        EntryKind::Data,
    );
    let placement_key = entry.did;
    assert!(matches!(
        receiver.dht().find_storage_owner(placement_key)?,
        PeerRingAction::Some(owner) if owner == receiver.did()
    ));
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(placement_key, entry.clone())],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        receiver.did(),
    )?;

    receiver_handler.handle(&context, &sync_msg).await?;

    let payload = next_payload(&sender).await?;
    match payload.transaction.data::<Message>()? {
        Message::SyncEntriesWithSuccessorReport(report) => {
            assert_eq!(report.acks, vec![SyncedEntryAck::new(
                placement_key,
                stored_entry.clone()
            )]);
        }
        message => {
            return Err(Error::InvalidMessage(format!(
                "expected SyncEntriesWithSuccessorReport, got {message:?}"
            )))
        }
    }
    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?,
        Some(stored_entry)
    );
    Ok(())
}

#[tokio::test]
async fn persist_synced_entries_returns_acks_for_owned_entries() -> Result<()> {
    let receiver = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = Entry::new(
        Did::from(10u32),
        vec!["acked".to_string().encode()?],
        EntryKind::Data,
    );
    let placement_key = entry.did;
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(placement_key, entry.clone())],
    };

    let acks = persist_synced_entries(&handler, &sync_msg).await?;

    assert_eq!(acks, vec![SyncedEntryAck::new(
        placement_key,
        stored_entry.clone()
    )]);
    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?,
        Some(stored_entry)
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_skips_entries_owned_by_another_virtual_owner() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let sender = prepare_node_with_virtual_nodes(next_generated_key(&mut keys)?, 2)?;
    let receiver = prepare_node_with_virtual_nodes(next_generated_key(&mut keys)?, 2)?;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    let _ = receiver.dht().join(sender.did())?;

    let placement_key = receiver
        .dht()
        .storage_virtual_positions(sender.did())?
        .into_iter()
        .next()
        .map(|position| position.vnode_did)
        .ok_or_else(|| Error::InvalidMessage("expected sender virtual position".to_string()))?;
    assert!(matches!(
        receiver.dht().find_storage_owner(placement_key)?,
        PeerRingAction::RemoteAction(owner, PeerRingRemoteAction::FindSuccessor(key))
            if owner == sender.did() && key == placement_key
    ));

    let entry = Entry::new(
        Did::from(10u32),
        vec!["wrong owner".to_string().encode()?],
        EntryKind::Data,
    );
    let stored_entry = entry.clone().try_into_storage_entry()?;
    sender
        .dht()
        .storage
        .put(&placement_key.to_string(), &stored_entry)
        .await?;
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(placement_key, entry)],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        receiver.did(),
    )?;
    sender.swarm.transport.record_pending_storage_sync_ack(
        context.transaction.tx_id,
        sync_msg.purpose,
        sync_msg.destination,
        receiver.did(),
        &sync_msg.data,
    )?;
    let receiver_handler =
        MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));

    receiver_handler.handle(&context, &sync_msg).await?;

    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?,
        None
    );
    let payload = next_payload_for_tx(&sender, context.transaction.tx_id).await?;
    match payload.transaction.data::<Message>()? {
        Message::SyncEntriesWithSuccessorReport(report) => {
            assert!(report.acks.is_empty());
        }
        message => {
            return Err(Error::InvalidMessage(format!(
                "expected SyncEntriesWithSuccessorReport, got {message:?}"
            )))
        }
    }
    assert_eq!(
        sender.dht().storage.get(&placement_key.to_string()).await?,
        Some(stored_entry)
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_physical_destination_routes_by_physical_did_not_storage_owner() -> Result<()>
{
    let mut keys = gen_ordered_keys(6).into_iter();
    let node = prepare_node_with_virtual_nodes(next_generated_key(&mut keys)?, 4)?;
    let mut peers = Vec::new();
    for _ in 0..5 {
        peers.push(next_generated_key(&mut keys)?.address().into());
    }
    for peer in peers.iter().copied() {
        let _ = node.dht().join(peer)?;
    }

    let dht = node.dht();
    let mut witness = None;
    for destination in peers {
        let physical_next = physical_sync_route_next_hop(&dht, destination)?;
        let storage_next = storage_sync_route_next_hop(&dht, destination)?;
        if physical_next != storage_next {
            witness = Some((destination, physical_next, storage_next));
            break;
        }
    }
    let Some((destination, physical_next, storage_next)) = witness else {
        return Err(Error::InvalidMessage(
            "expected physical and storage routes to diverge".to_string(),
        ));
    };

    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(destination),
        data: vec![],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(msg.clone()),
        node.swarm.transport.session_sk(),
        node.did(),
        destination,
    )?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));

    let next = next_hop_for_sync_entries(&handler, &context, &msg)?;

    assert_eq!(next, physical_next);
    assert_ne!(next, storage_next);
    Ok(())
}
