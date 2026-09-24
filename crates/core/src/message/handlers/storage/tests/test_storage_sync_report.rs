use std::sync::Arc;

use super::super::next_hop_for_sync_entries;
use super::test_support::next_generated_key;
use super::test_support::next_payload_for_tx;
use super::test_support::physical_sync_route_next_hop;
use super::test_support::prepare_node_with_virtual_nodes;
use super::test_support::storage_sync_route_next_hop;
use super::test_support::NoopCallback;
use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::entry::PlacedEntry;
use crate::dht::entry::SyncedEntryAck;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::dht::StorageKey;
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
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::Node;
use crate::tests::held_inbox_for;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;

#[tokio::test]
async fn test_sync_entries_handler_reports_persisted_entries() -> Result<()> {
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
    let entry = live_entry(
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
        sender.swarm.transport.message_signer(),
        receiver.did(),
        receiver.did(),
    )?;

    receiver_handler.handle(&context, &sync_msg).await?;

    let payload = next_payload_for_tx(&sender, context.transaction.tx_id).await?;
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

/// A witnessed inbox carrier for `destination`, held now by a fresh session.
fn inbox_held_by_a_stranger(destination: Did) -> Result<Entry> {
    held_inbox_for(
        destination,
        &DelegateeKey::new_with_seckey(&SecretKey::random())?,
    )
}

/// The slot of the inbox carrier `inbox`.
fn inbox_slot(inbox: &Entry) -> StorageKey {
    StorageKey::new(EntryKind::RelayMessage, inbox.did)
}

/// Relocation law: a relay carrier is accepted from the receiver's predecessor and skipped
/// without an acknowledgement from anyone else, while the data entries sharing its batch are
/// accepted either way; the carrier is not invalid, only not this receiver's to take yet.
#[tokio::test]
async fn test_persist_synced_entries_relocates_a_relay_carrier_from_the_predecessor_alone(
) -> Result<()> {
    let receiver = prepare_node(SecretKey::random()).await;
    let predecessor: Did = SecretKey::random().address().into();
    let stranger: Did = SecretKey::random().address().into();
    *receiver.dht().lock_predecessor()? = Some(predecessor);
    let topic = live_entry(
        Did::from(10u32),
        vec!["acked".to_string().encode()?],
        EntryKind::Data,
    );
    let inbox = inbox_held_by_a_stranger(receiver.did())?;
    let sync_msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(receiver.did()),
        data: vec![
            PlacedEntry::new(inbox.did, inbox.clone()),
            PlacedEntry::new(topic.did, topic.clone()),
        ],
    };

    let from_stranger = receiver
        .swarm
        .transport
        .persist_storage_sync_entries(&sync_msg, stranger)
        .await?;
    assert_eq!(from_stranger, vec![SyncedEntryAck::new(
        topic.did,
        topic.clone().try_into_storage_entry()?
    )]);
    assert_eq!(
        receiver
            .dht()
            .storage
            .get(&inbox_slot(&inbox).to_string())
            .await?,
        None
    );

    let from_predecessor = receiver
        .swarm
        .transport
        .persist_storage_sync_entries(&sync_msg, predecessor)
        .await?;
    assert_eq!(from_predecessor.len(), 2);
    assert!(receiver
        .dht()
        .storage
        .get(&inbox_slot(&inbox).to_string())
        .await?
        .is_some());
    Ok(())
}

#[tokio::test]
async fn test_persist_synced_entries_returns_acks_for_owned_entries() -> Result<()> {
    let receiver = prepare_node(SecretKey::random()).await;
    let entry = live_entry(
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

    let acks = receiver
        .swarm
        .transport
        .persist_storage_sync_entries(&sync_msg, Did::from(1u32))
        .await?;

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
async fn test_sync_entries_handler_skips_entries_owned_by_another_virtual_owner() -> Result<()> {
    let mut keys = gen_ordered_keys::<2>().into_iter();
    let sender = prepare_node_with_virtual_nodes(next_generated_key(&mut keys)?, 2)?;
    let receiver = prepare_node_with_virtual_nodes(next_generated_key(&mut keys)?, 2)?;
    manually_establish_connection(&sender.swarm, &receiver.swarm).await;
    wait_for_msgs([&sender, &receiver]).await;
    assert_no_more_msg([&sender, &receiver]).await;
    let _ = receiver.dht().admit_connected(sender.did(), None)?;

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

    let entry = live_entry(
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
        sender.swarm.transport.message_signer(),
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

/// A fixture key: the secret scalar `scalar`, so its address, and hence its ring position and
/// virtual positions, are fixed.
fn fixture_key(scalar: u8) -> Result<SecretKey> {
    SecretKey::try_from(format!("{scalar:064x}").as_str())
}

/// The ring of the physical-destination fixture: the local node (scalar `0x10`, 4 virtual
/// positions per owner, finger table size 8) with the peers of scalars `0x11..=0x15` admitted.
fn physical_destination_fixture() -> Result<(Node, [Did; 5])> {
    let node = prepare_node_with_virtual_nodes(fixture_key(0x10)?, 4)?;
    let mut peers = [Did::default(); 5];
    for (peer, scalar) in peers.iter_mut().zip(0x11..=0x15) {
        *peer = fixture_key(scalar)?.address().into();
        let _ = node.dht().admit_connected(*peer, None)?;
    }
    Ok((node, peers))
}

/// Route `destination` as `PhysicalOwner` through `node`'s sync handler.
fn physical_owner_next_hop(node: &Node, destination: Did) -> Result<Option<Did>> {
    let msg = SyncEntriesWithSuccessor {
        purpose: StorageSyncPurpose::OwnershipHandoff,
        destination: StorageSyncDestination::PhysicalOwner(destination),
        data: vec![],
    };
    let context = MessagePayload::new_send(
        Message::SyncEntriesWithSuccessor(msg.clone()),
        node.swarm.transport.message_signer(),
        node.did(),
        destination,
    )?;
    let handler = MessageHandler::new(node.swarm.transport.clone(), Arc::new(NoopCallback));
    next_hop_for_sync_entries(&handler, &context, &msg)
}

/// Law: a `PhysicalOwner` sync routes by the physical DID, never by the storage owner of its
/// position (#862: deterministic fixture instead of a random draw).
///
/// Fixture (`physical_destination_fixture`), addresses abbreviated to their first 16 bits:
///
/// ```text
/// local 0x10 → fae3 ; peers 0x11 → 252d, 0x12 → 7919, 0x13 → 4bd1, 0x14 → 811d, 0x15 → 157b
/// successors(local) = [157b, 252d, 4bd1]            \* clockwise from fae3, capacity 3
/// virtual positions around the diverging key:  … 4428 (252d), 457d (local), 568e (4bd1) …
/// virtual positions around the coinciding key: … 568e (4bd1), a888 (157b) …
///
/// destination 4bd1 (0x13): physical = find_successor(4bd1) from fae3 lies past the head
///                          157b, so the next hop is 157b;
///                          storage  = owner of the first position ≥ 4bd1, 568e, is 4bd1.
///                          physical ≠ storage: the diverging witness.
/// destination 7919 (0x12): physical = 157b; storage = owner of a888, 157b: they coincide.
/// ```
///
/// Both premises are asserted, so a change to key derivation or virtual placement fails here
/// deterministically instead of making the witness disappear at random.
#[tokio::test]
async fn test_sync_entries_physical_destination_routes_by_physical_did_not_storage_owner(
) -> Result<()> {
    let (node, peers) = physical_destination_fixture()?;
    let dht = node.dht();
    let [_, coinciding, diverging, _, head] = peers;

    assert_eq!(physical_sync_route_next_hop(&dht, diverging)?, Some(head));
    assert_eq!(
        storage_sync_route_next_hop(&dht, diverging)?,
        Some(diverging)
    );
    assert_eq!(physical_owner_next_hop(&node, diverging)?, Some(head));

    assert_eq!(physical_sync_route_next_hop(&dht, coinciding)?, Some(head));
    assert_eq!(storage_sync_route_next_hop(&dht, coinciding)?, Some(head));
    assert_eq!(physical_owner_next_hop(&node, coinciding)?, Some(head));
    Ok(())
}
