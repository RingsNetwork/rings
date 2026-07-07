use std::sync::Arc;

use super::super::finish_storage_action;
use super::test_support::install_two_node_chord_view;
use super::test_support::next_generated_key;
use super::test_support::next_payload;
use super::test_support::next_payload_for_tx;
use super::test_support::non_affine_placement;
use super::test_support::prepare_node_with_storage_redundancy;
use super::test_support::remote_storage_placement_after;
use super::test_support::NoopCallback;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::dht::entry::Entry;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::StorageSyncDestination;
use crate::dht::StorageSyncPurpose;
use crate::ecc::tests::gen_ordered_keys;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessor;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::Encoder;
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::prelude::entry::EntryKind;
use crate::session::SessionSk;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::manually_establish_connection;

#[test]
fn finish_storage_action_accepts_empty_action() -> Result<()> {
    finish_storage_action(PeerRingAction::None)?;
    Ok(())
}

#[test]
fn finish_storage_action_rejects_unhandled_action() -> Result<()> {
    let did = SecretKey::random().address().into();
    match finish_storage_action(PeerRingAction::Some(did)) {
        Err(Error::PeerRingUnexpectedAction(action)) => {
            assert_eq!(*action, PeerRingAction::Some(did));
            Ok(())
        }
        res => Err(Error::InvalidMessage(format!(
            "expected unexpected storage action, got {res:?}"
        ))),
    }
}

#[tokio::test]
async fn sync_entries_handler_stores_entry_at_placement_key() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource_id = Did::from(10u32);
    let entry = Entry::new(
        resource_id,
        vec!["placed".to_string().encode()?],
        EntryKind::Data,
    );
    let placement_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected redundant placement".to_string()))?;
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        &context_session,
        node.did(),
        node.did(),
    )?;

    handler
        .handle(&context, &SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::OwnershipHandoff,
            destination: StorageSyncDestination::PhysicalOwner(node.did()),
            data: vec![PlacedEntry::new(placement_key, entry.clone())],
        })
        .await?;

    assert_eq!(
        node.dht().storage.get(&placement_key.to_string()).await?,
        Some(stored_entry)
    );
    assert_eq!(
        node.dht().storage.get(&resource_id.to_string()).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_caps_inbound_entry_payloads() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = Entry::new(
        Did::from(10u32),
        (0..ENTRY_DATA_MAX_LEN + 3)
            .map(|i| format!("payload{i}").encode())
            .collect::<Result<Vec<_>>>()?,
        EntryKind::Data,
    );
    let placement_key = entry.did;
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        &context_session,
        node.did(),
        node.did(),
    )?;

    handler
        .handle(&context, &SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::OwnershipHandoff,
            destination: StorageSyncDestination::PhysicalOwner(node.did()),
            data: vec![PlacedEntry::new(placement_key, entry)],
        })
        .await?;

    let stored = node
        .dht()
        .storage
        .get(&placement_key.to_string())
        .await?
        .ok_or_else(|| Error::InvalidMessage("expected stored sync entry".to_string()))?;

    assert_eq!(stored.data.len(), ENTRY_DATA_MAX_LEN);
    let first_payload: String = stored
        .data
        .first()
        .ok_or_else(|| Error::InvalidMessage("expected capped payload".to_string()))?
        .decode()?;
    assert_eq!(first_payload, String::from("payload3"));
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_rejects_non_affine_placement_before_writing() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let valid_entry = Entry::new(
        Did::from(20u32),
        vec!["valid".to_string().encode()?],
        EntryKind::Data,
    );
    let invalid_entry = Entry::new(
        Did::from(10u32),
        vec!["invalid".to_string().encode()?],
        EntryKind::Data,
    );
    let valid_placement = valid_entry.did;
    let invalid_placement = non_affine_placement(invalid_entry.did, 2)?;
    let context_key = SecretKey::random();
    let context_session = SessionSk::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        &context_session,
        node.did(),
        node.did(),
    )?;

    let result = handler
        .handle(&context, &SyncEntriesWithSuccessor {
            purpose: StorageSyncPurpose::OwnershipHandoff,
            destination: StorageSyncDestination::PhysicalOwner(node.did()),
            data: vec![
                PlacedEntry::new(valid_placement, valid_entry),
                PlacedEntry::new(invalid_placement, invalid_entry),
            ],
        })
        .await;

    assert!(matches!(result, Err(Error::InvalidMessage(_))));
    assert_eq!(
        node.dht().storage.get(&valid_placement.to_string()).await?,
        None
    );
    assert_eq!(
        node.dht()
            .storage
            .get(&invalid_placement.to_string())
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_accepts_placement_destination_on_local_branch() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let node1 = prepare_node(next_generated_key(&mut keys)?).await;
    let node2 = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;
    assert_no_more_msg([&node1, &node2]).await;
    install_two_node_chord_view(&node1, &node2)?;

    let handler = MessageHandler::new(node1.swarm.transport.clone(), Arc::new(NoopCallback));
    let placement_key = node2.did();
    assert!(matches!(
        node1.dht().find_storage_owner(placement_key)?,
        PeerRingAction::Some(witness) if witness == node2.did() && witness != node1.did()
    ));
    let entry = Entry::new(
        placement_key,
        vec!["routed repair".to_string().encode()?],
        EntryKind::Data,
    );
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PlacementKey(placement_key),
        data: vec![PlacedEntry::new(placement_key, entry.clone())],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(msg.clone()),
        node2.swarm.transport.session_sk(),
        node1.did(),
        placement_key,
    )?;

    handler.handle(&context, &msg).await?;

    assert_eq!(
        node1.dht().storage.get(&placement_key.to_string()).await?,
        Some(stored_entry.clone())
    );
    let ack = next_payload(&node2).await?;
    assert!(matches!(
        ack.transaction.data()?,
        Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport {
            purpose,
            destination,
            receiver,
            acks
        }) if purpose == StorageSyncPurpose::OwnershipHandoff
            && destination == StorageSyncDestination::PlacementKey(placement_key)
            && receiver == node1.did()
            && acks == vec![SyncedEntryAck::new(placement_key, stored_entry)]
    ));
    Ok(())
}

#[tokio::test]
async fn additive_repair_sync_persists_without_cleanup_report() -> Result<()> {
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
    let placement_key = Did::from(10u32);
    assert!(matches!(
        receiver.dht().find_storage_owner(placement_key)?,
        PeerRingAction::Some(owner) if owner == receiver.did()
    ));
    let entry = Entry::new(
        placement_key,
        vec!["repair copy".to_string().encode()?],
        EntryKind::Data,
    );
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::AdditiveRepair,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(placement_key, entry)],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        receiver.did(),
    )?;

    receiver_handler.handle(&context, &sync_msg).await?;

    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?,
        Some(stored_entry)
    );
    assert_no_more_msg([&sender]).await;
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_rejects_mismatched_placement_destination() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let sender = prepare_node(next_generated_key(&mut keys)?).await;
    let receiver = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    install_two_node_chord_view(&sender, &receiver)?;

    let destination_key = receiver.did();
    let mismatched_key = sender.did();
    let entry = Entry::new(
        Did::from(10u32),
        vec!["mismatched placement".to_string().encode()?],
        EntryKind::Data,
    );
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PlacementKey(destination_key),
        data: vec![PlacedEntry::new(mismatched_key, entry)],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.session_sk(),
        receiver.did(),
        destination_key,
    )?;
    let receiver_handler =
        MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));

    receiver_handler.handle(&context, &sync_msg).await?;

    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&mismatched_key.to_string())
            .await?,
        None
    );
    let payload = next_payload_for_tx(&sender, context.transaction.tx_id).await?;
    assert!(matches!(
        payload.transaction.data::<Message>()?,
        Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport { acks, .. })
            if acks.is_empty()
    ));
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_rejects_physical_destination_for_unowned_placement() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let sender = prepare_node(next_generated_key(&mut keys)?).await;
    let receiver = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    install_two_node_chord_view(&sender, &receiver)?;

    let placement_key = remote_storage_placement_after(&receiver, sender.did())?;
    let entry = Entry::new(
        Did::from(10u32),
        vec!["wrong physical owner".to_string().encode()?],
        EntryKind::Data,
    );
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
    assert!(matches!(
        payload.transaction.data::<Message>()?,
        Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport { acks, .. })
            if acks.is_empty()
    ));
    Ok(())
}

#[tokio::test]
async fn sync_entries_handler_acks_local_branch_with_successor_witness() -> Result<()> {
    let mut keys = gen_ordered_keys(2).into_iter();
    let sender = prepare_node(next_generated_key(&mut keys)?).await;
    let receiver = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    install_two_node_chord_view(&sender, &receiver)?;

    let placement_key = sender.did();
    assert!(matches!(
        receiver.dht().find_storage_owner(placement_key)?,
        PeerRingAction::Some(witness) if witness == sender.did() && witness != receiver.did()
    ));

    let entry = Entry::new(
        placement_key,
        vec!["successor witness owner".to_string().encode()?],
        EntryKind::Data,
    );
    let stored_entry = entry.clone().try_into_storage_entry()?;
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
    let receiver_handler =
        MessageHandler::new(receiver.swarm.transport.clone(), Arc::new(NoopCallback));

    receiver_handler.handle(&context, &sync_msg).await?;

    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&placement_key.to_string())
            .await?,
        Some(stored_entry.clone())
    );
    let payload = next_payload_for_tx(&sender, context.transaction.tx_id).await?;
    assert!(matches!(
        payload.transaction.data::<Message>()?,
        Message::SyncEntriesWithSuccessorReport(SyncEntriesWithSuccessorReport { acks, .. })
            if acks == vec![SyncedEntryAck::new(placement_key, stored_entry)]
    ));
    Ok(())
}
