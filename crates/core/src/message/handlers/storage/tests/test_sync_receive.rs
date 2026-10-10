use std::sync::Arc;

use bytes::Bytes;

use super::super::finish_storage_action;
use super::test_support::install_two_node_chord_view;
use super::test_support::next_generated_key;
use super::test_support::next_payload_for_tx;
use super::test_support::non_affine_placement;
use super::test_support::prepare_node_with_storage_redundancy;
use super::test_support::remote_storage_placement_after;
use super::test_support::NoopCallback;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::delegation::DelegateeKey;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
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
use crate::message::HandleMsg;
use crate::message::MessageHandler;
use crate::message::MessagePayload;
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::swarm::transport::StorageSyncBatch;
use crate::swarm::transport::StorageSyncBatchStep;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;
use crate::tests::TEST_NETWORK_ID;
use crate::utils::get_epoch_ms;

#[test]
fn test_finish_storage_action_accepts_empty_action() -> Result<()> {
    finish_storage_action(PeerRingAction::None)?;
    Ok(())
}

#[test]
fn test_finish_storage_action_rejects_unhandled_action() -> Result<()> {
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
async fn test_sync_entries_handler_stores_entry_at_placement_key() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let resource_id = Did::from(10u32);
    let entry = live_entry(resource_id, vec![Bytes::from("placed")], EntryKind::Data);
    let placement_key = entry
        .did
        .rotate_affine(2)?
        .into_iter()
        .nth(1)
        .ok_or_else(|| Error::InvalidMessage("expected redundant placement".to_string()))?;
    let stored_entry = entry.clone().try_into_storage_entry()?;
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_sync_entries_handler_caps_inbound_entry_payloads() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let entry = live_entry(
        Did::from(10u32),
        (0..ENTRY_DATA_MAX_LEN + 3)
            .map(|i| Bytes::from(format!("payload{i}")))
            .collect::<Vec<_>>(),
        EntryKind::Data,
    );
    let placement_key = entry.did;
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
    let first_payload = stored
        .data
        .first()
        .ok_or_else(|| Error::InvalidMessage("expected capped payload".to_string()))?;
    assert_eq!(first_payload, &Bytes::from("payload3"));
    Ok(())
}

#[tokio::test]
async fn test_sync_entries_handler_rejects_non_affine_placement_before_writing() -> Result<()> {
    let node = prepare_node_with_storage_redundancy(SecretKey::random(), 2)?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    let valid_entry = live_entry(
        Did::from(20u32),
        vec![Bytes::from("valid")],
        EntryKind::Data,
    );
    let invalid_entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("invalid")],
        EntryKind::Data,
    );
    let valid_placement = valid_entry.did;
    let invalid_placement = non_affine_placement(invalid_entry.did, 2)?;
    let context_key = SecretKey::random();
    let context_session = DelegateeKey::new_with_seckey(&context_key)?;
    let context = MessagePayload::new_send(
        Message::custom(b"sync context")?,
        MessageSigner::new(&context_session, TEST_NETWORK_ID),
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
async fn test_storage_sync_batch_persists_one_entry_per_step_after_validation() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let first = live_entry(
        Did::from(31u32),
        vec![Bytes::from("first")],
        EntryKind::Data,
    );
    let second = live_entry(
        Did::from(32u32),
        vec![Bytes::from("second")],
        EntryKind::Data,
    );
    let first_key = first.did;
    let second_key = second.did;
    let first_stored = first.clone().try_into_storage_entry()?;
    let second_stored = second.clone().try_into_storage_entry()?;
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(node.did()),
        data: vec![
            PlacedEntry::new(first_key, first),
            PlacedEntry::new(second_key, second),
        ],
    };
    let mut batch = StorageSyncBatch::new(&msg, Did::from(1u32), get_epoch_ms());

    assert!(matches!(
        batch.step(&node.swarm.transport).await?,
        StorageSyncBatchStep::Pending
    ));
    assert_eq!(node.dht().storage.get(&first_key.to_string()).await?, None);
    assert_eq!(node.dht().storage.get(&second_key.to_string()).await?, None);

    assert!(matches!(
        batch.step(&node.swarm.transport).await?,
        StorageSyncBatchStep::Pending
    ));
    assert_eq!(node.dht().storage.get(&first_key.to_string()).await?, None);
    assert_eq!(node.dht().storage.get(&second_key.to_string()).await?, None);

    assert!(matches!(
        batch.step(&node.swarm.transport).await?,
        StorageSyncBatchStep::Pending
    ));
    assert_eq!(
        node.dht().storage.get(&first_key.to_string()).await?,
        Some(first_stored.clone())
    );
    assert_eq!(node.dht().storage.get(&second_key.to_string()).await?, None);

    let final_step = batch.step(&node.swarm.transport).await?;
    let persisted_second = node.dht().storage.get(&second_key.to_string()).await?;
    assert_eq!(persisted_second.as_ref(), Some(&second_stored));

    match final_step {
        StorageSyncBatchStep::Complete(acks) => {
            assert_eq!(acks, vec![
                SyncedEntryAck::new(first_key, first_stored),
                SyncedEntryAck::new(second_key, second_stored),
            ]);
        }
        StorageSyncBatchStep::Pending => {
            return Err(Error::InvalidMessage(
                "expected storage sync batch completion".to_string(),
            ))
        }
    }
    Ok(())
}

/// An entry the receiver cannot admit (here one already past its retention bound on this clock,
/// as an entry live on the sender's clock can be up to the skew tolerance later) is skipped
/// without an ack and holds back no other entry of its batch: the admissible one is persisted
/// and acknowledged, the inadmissible one is neither, so its owner keeps it.
#[tokio::test]
async fn test_storage_sync_batch_skips_an_inadmissible_entry_without_failing_the_batch(
) -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let now = get_epoch_ms();
    let admissible = live_entry(Did::from(41u32), vec![Bytes::from("kept")], EntryKind::Data);
    let mut expired = live_entry(Did::from(42u32), vec![Bytes::from("gone")], EntryKind::Data);
    expired.expires_at_ms = Some(now - 1);
    let (admissible_key, expired_key) = (admissible.did, expired.did);
    let admissible_stored = admissible.clone().try_into_storage_entry()?;
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(node.did()),
        data: vec![
            PlacedEntry::new(expired_key, expired),
            PlacedEntry::new(admissible_key, admissible),
        ],
    };
    let mut batch = StorageSyncBatch::new(&msg, Did::from(1u32), now);
    let acks = loop {
        if let StorageSyncBatchStep::Complete(acks) = batch.step(&node.swarm.transport).await? {
            break acks;
        }
    };

    assert_eq!(acks, vec![SyncedEntryAck::new(
        admissible_key,
        admissible_stored.clone()
    )]);
    assert_eq!(
        node.dht().storage.get(&admissible_key.to_string()).await?,
        Some(admissible_stored)
    );
    assert_eq!(
        node.dht().storage.get(&expired_key.to_string()).await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn test_sync_entries_handler_accepts_placement_destination_on_local_branch() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
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
    let entry = live_entry(
        placement_key,
        vec![Bytes::from("routed repair")],
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
        node2.swarm.transport.message_signer(),
        node1.did(),
        placement_key,
    )?;

    handler.handle(&context, &msg).await?;

    assert_eq!(
        node1.dht().storage.get(&placement_key.to_string()).await?,
        Some(stored_entry.clone())
    );
    let ack = next_payload_for_tx(&node2, context.transaction.tx_id).await?;
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
async fn test_additive_repair_sync_persists_without_cleanup_report() -> Result<()> {
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
    let entry = live_entry(
        placement_key,
        vec![Bytes::from("repair copy")],
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
        sender.swarm.transport.message_signer(),
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
async fn test_sync_entries_handler_rejects_mismatched_placement_destination() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let sender = prepare_node(next_generated_key(&mut keys)?).await;
    let receiver = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    install_two_node_chord_view(&sender, &receiver)?;

    let destination_key = receiver.did();
    let mismatched_key = sender.did();
    let entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("mismatched placement")],
        EntryKind::Data,
    );
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PlacementKey(destination_key),
        data: vec![PlacedEntry::new(mismatched_key, entry)],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.message_signer(),
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
async fn test_sync_entries_handler_rejects_physical_destination_for_unowned_placement() -> Result<()>
{
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let sender = prepare_node(next_generated_key(&mut keys)?).await;
    let receiver = prepare_node(next_generated_key(&mut keys)?).await;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    install_two_node_chord_view(&sender, &receiver)?;

    let placement_key = remote_storage_placement_after(&receiver, sender.did())?;
    let entry = live_entry(
        Did::from(10u32),
        vec![Bytes::from("wrong physical owner")],
        EntryKind::Data,
    );
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![PlacedEntry::new(placement_key, entry)],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(sync_msg.clone()),
        sender.swarm.transport.message_signer(),
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
async fn test_sync_entries_handler_acks_local_branch_with_successor_witness() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
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

    let entry = live_entry(
        placement_key,
        vec![Bytes::from("successor witness owner")],
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
        sender.swarm.transport.message_signer(),
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
