use std::sync::Arc;

use tokio::time::timeout;
use tokio::time::Duration;

use super::super::ChordStorageInterfaceCacheChecker;
use crate::dht::entry::Entry;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::PeerRingAction;
use crate::dht::PeerRingRemoteAction;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::message::types::Message;
use crate::message::types::SyncEntriesWithSuccessorReport;
use crate::message::Encoder;
use crate::message::MessagePayload;
use crate::message::MessageRelay;
use crate::message::Transaction;
use crate::prelude::entry::EntryKind;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::SwarmBuilder;
use crate::tests::default::Node;

pub(super) struct NoopCallback;

impl SwarmCallback for NoopCallback {}

pub(super) async fn next_payload(node: &Node) -> Result<MessagePayload> {
    node.listen_once()
        .await
        .ok_or_else(|| Error::InvalidMessage("expected message payload".to_string()))
}

pub(super) async fn next_payload_for_tx(node: &Node, tx_id: uuid::Uuid) -> Result<MessagePayload> {
    for _ in 0..64 {
        let payload = timeout(Duration::from_secs(1), node.listen_once())
            .await
            .map_err(|_| {
                Error::InvalidMessage("timed out waiting for matching payload".to_string())
            })?
            .ok_or_else(|| Error::InvalidMessage("expected message payload".to_string()))?;
        if payload.transaction.tx_id == tx_id {
            return Ok(payload);
        }
    }

    Err(Error::InvalidMessage(
        "matching transaction payload was not observed".to_string(),
    ))
}

pub(super) fn next_generated_key(keys: &mut impl Iterator<Item = SecretKey>) -> Result<SecretKey> {
    keys.next()
        .ok_or_else(|| Error::InvalidMessage("expected generated key".to_string()))
}

pub(super) fn storage_sync_report_payload(
    request: &MessagePayload,
    report: SyncEntriesWithSuccessorReport,
    signer: &SessionSk,
    next_hop: Did,
    destination: Did,
) -> Result<MessagePayload> {
    let transaction = Transaction::new(
        destination,
        request.transaction.tx_id,
        Message::SyncEntriesWithSuccessorReport(report),
        signer,
    )?;
    let relay = MessageRelay::new(vec![signer.account_did()], next_hop, destination);
    MessagePayload::new(transaction, signer, relay)
}

pub(super) fn prepare_node_with_storage_redundancy(
    key: SecretKey,
    redundancy: u16,
) -> Result<Node> {
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session_sk,
        )
        .dht_storage_redundancy(redundancy)
        .dht_finger_table_size(8)
        .build(),
    );
    Ok(Node::new(swarm))
}

pub(super) fn prepare_node_with_virtual_nodes(
    key: SecretKey,
    positions_per_peer: u16,
) -> Result<Node> {
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session_sk,
        )
        .dht_virtual_nodes(positions_per_peer)
        .dht_finger_table_size(8)
        .build(),
    );
    Ok(Node::new(swarm))
}

pub(super) fn owner_index(nodes: &[&Node], placement: Did) -> Result<usize> {
    let mut owner = None;
    for (index, node) in nodes.iter().enumerate() {
        if !matches!(
            node.dht().find_successor(placement)?,
            PeerRingAction::Some(_)
        ) {
            continue;
        }

        if owner.replace(index).is_some() {
            return Err(Error::InvalidMessage(
                "placement has more than one observed owner".to_string(),
            ));
        }
    }
    owner.ok_or_else(|| Error::InvalidMessage("placement has no observed owner".to_string()))
}

pub(super) fn physical_sync_route_next_hop(
    dht: &PeerRing,
    destination: Did,
) -> Result<Option<Did>> {
    if destination == dht.did {
        return Ok(None);
    }

    match dht.find_successor(destination)? {
        PeerRingAction::Some(next) if next == dht.did => Ok(Some(destination)),
        PeerRingAction::Some(next) => Ok(Some(next)),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(_)) => {
            Ok(Some(next))
        }
        action => Err(Error::unexpected_peer_ring_action(action)),
    }
}

pub(super) fn storage_sync_route_next_hop(dht: &PeerRing, placement: Did) -> Result<Option<Did>> {
    match dht.find_storage_owner(placement)? {
        PeerRingAction::Some(_) => Ok(None),
        PeerRingAction::RemoteAction(next, PeerRingRemoteAction::FindSuccessor(_)) => {
            Ok(Some(next))
        }
        action => Err(Error::unexpected_peer_ring_action(action)),
    }
}

pub(super) fn remote_storage_placement_after(node: &Node, start: Did) -> Result<Did> {
    for offset in 1..512 {
        let placement = start + Did::from(offset);
        if matches!(
            node.dht().find_storage_owner(placement)?,
            PeerRingAction::RemoteAction(_, PeerRingRemoteAction::FindSuccessor(key))
                if key == placement
        ) {
            return Ok(placement);
        }
    }

    Err(Error::InvalidMessage(
        "expected a remote storage placement".to_string(),
    ))
}

pub(super) fn install_two_node_chord_view(first: &Node, second: &Node) -> Result<()> {
    first.dht().successors().update(second.did())?;
    second.dht().successors().update(first.did())?;
    *first.dht().lock_predecessor()? = Some(second.did());
    *second.dht().lock_predecessor()? = Some(first.did());
    Ok(())
}

pub(super) fn split_redundant_entry(nodes: &[&Node]) -> Result<(Entry, Did, Did, usize, usize)> {
    for attempt in 0..512 {
        let topic = format!("split remote replica placement {attempt}");
        let entry: Entry = topic.try_into()?;
        let mut placements = entry.did.rotate_affine(2)?.into_iter();
        let primary = placements
            .next()
            .ok_or_else(|| Error::InvalidMessage("expected primary placement".to_string()))?;
        let replica = placements
            .next()
            .ok_or_else(|| Error::InvalidMessage("expected replica placement".to_string()))?;
        let primary_owner = owner_index(nodes, primary)?;
        let replica_owner = owner_index(nodes, replica)?;
        if primary_owner != replica_owner {
            return Ok((entry, primary, replica, primary_owner, replica_owner));
        }
    }

    Err(Error::InvalidMessage(
        "could not sample a split-owner redundant entry".to_string(),
    ))
}

pub(super) fn non_affine_placement(entry_key: Did, redundancy: u16) -> Result<Did> {
    let placements = entry_key.rotate_affine(redundancy)?;
    for attempt in 0..512 {
        let candidate = Entry::gen_did(&format!("non-affine placement {attempt}"))?;
        if !placements.contains(&candidate) {
            return Ok(candidate);
        }
    }

    Err(Error::InvalidMessage(
        "could not sample non-affine placement".to_string(),
    ))
}

pub(super) async fn assert_cached_data_values(
    node: &Node,
    entry_key: Did,
    expected: &[&str],
) -> Result<()> {
    let entry = node
        .swarm
        .storage_check_cache(entry_key)
        .await
        .ok_or_else(|| Error::InvalidMessage("expected cached entry".to_string()))?;
    let expected_data = expected
        .iter()
        .map(|value| value.to_string().encode())
        .collect::<Result<Vec<_>>>()?;

    assert_eq!(entry.did, entry_key);
    assert_eq!(entry.kind, EntryKind::Data);
    assert_eq!(entry.data, expected_data);
    assert_eq!(entry.crdt.dots.len(), entry.data.len());
    Ok(())
}
