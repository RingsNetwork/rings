use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt;
use tokio::time::timeout;
use tokio::time::Duration;
use tokio::time::Instant;

use super::super::ChordStorageInterfaceCacheChecker;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
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
use crate::message::MessageSigner;
use crate::message::PayloadSender;
use crate::message::Transaction;
use crate::session::SessionSk;
use crate::storage::MemStorage;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::SwarmBuilder;
use crate::tests::default::Node;
use crate::tests::default::TEST_NETWORK_IDLE_TIMEOUT;

pub(super) struct NoopCallback;

impl SwarmCallback for NoopCallback {}

fn payload_observation_error(label: &str, observed: &[MessagePayload]) -> Error {
    let observed = observed
        .iter()
        .map(|payload| {
            format!(
                "tx_id={}, destination={}, message={:?}",
                payload.transaction.tx_id,
                payload.transaction.destination,
                payload.transaction.data::<Message>()
            )
        })
        .collect::<Vec<_>>();
    Error::InvalidMessage(format!(
        "timed out waiting for {label}; unmatched payloads={observed:?}"
    ))
}

struct PayloadRestoreGuard<'scan, 'inbox> {
    scan: &'scan mut crate::tests::default::NodeMessageScan<'inbox>,
    payload: MessagePayload,
    restore: bool,
}

impl<'scan, 'inbox> PayloadRestoreGuard<'scan, 'inbox> {
    fn new(
        scan: &'scan mut crate::tests::default::NodeMessageScan<'inbox>,
        payload: MessagePayload,
    ) -> Self {
        Self {
            scan,
            payload,
            restore: true,
        }
    }

    const fn payload(&self) -> &MessagePayload {
        &self.payload
    }

    fn accept(mut self) -> MessagePayload {
        self.restore = false;
        self.payload.clone()
    }
}

impl Drop for PayloadRestoreGuard<'_, '_> {
    fn drop(&mut self) {
        if self.restore {
            self.scan.skip(self.payload.clone());
        }
    }
}

pub(super) async fn next_payload_matching(
    node: &Node,
    label: &str,
    matches: impl FnMut(&MessagePayload) -> Result<bool>,
) -> Result<MessagePayload> {
    next_payload_matching_with_timeout(node, label, TEST_NETWORK_IDLE_TIMEOUT, matches).await
}

async fn next_payload_matching_with_timeout(
    node: &Node,
    label: &str,
    observation_timeout: Duration,
    mut matches: impl FnMut(&MessagePayload) -> Result<bool>,
) -> Result<MessagePayload> {
    let deadline = Instant::now() + observation_timeout;
    let mut scan = node.message_scan().await;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(payload_observation_error(label, scan.skipped()));
        }
        let payload = match timeout(remaining, scan.next()).await {
            Ok(Some(payload)) => payload,
            Ok(None) => {
                return Err(Error::InvalidMessage(
                    "message payload channel closed while waiting for test observation".to_string(),
                ));
            }
            Err(_) => {
                return Err(payload_observation_error(label, scan.skipped()));
            }
        };
        let candidate = PayloadRestoreGuard::new(&mut scan, payload);
        match matches(candidate.payload()) {
            Ok(true) => return Ok(candidate.accept()),
            Ok(false) => {}
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn next_payload_for_tx(node: &Node, tx_id: uuid::Uuid) -> Result<MessagePayload> {
    next_payload_matching(node, "matching transaction payload", |payload| {
        Ok(payload.transaction.tx_id == tx_id)
    })
    .await
}

#[tokio::test]
async fn test_matching_payload_restores_skipped_messages_in_order() -> Result<()> {
    let node = crate::tests::default::prepare_node(SecretKey::random()).await;
    let first = test_payload(&node, b"first")?;
    let second = test_payload(&node, b"second")?;
    let matched = test_payload(&node, b"matched")?;
    node.prepend_messages_for_test(vec![first.clone(), second.clone(), matched.clone()])
        .await;

    let observed = next_payload_for_tx(&node, matched.transaction.tx_id).await?;

    assert_eq!(observed, matched);
    assert_eq!(node.try_listen_once().await, Some(first));
    assert_eq!(node.try_listen_once().await, Some(second));
    assert!(node.try_listen_once().await.is_none());
    Ok(())
}

#[tokio::test]
async fn test_matching_payload_restores_skipped_message_after_predicate_error() -> Result<()> {
    let node = crate::tests::default::prepare_node(SecretKey::random()).await;
    let skipped = test_payload(&node, b"predicate error")?;
    node.prepend_messages_for_test(vec![skipped.clone()]).await;

    let result = next_payload_matching_with_timeout(
        &node,
        "predicate error",
        Duration::from_millis(20),
        |_| Err(Error::InvalidMessage("predicate failed".to_string())),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(node.try_listen_once().await, Some(skipped));
    Ok(())
}

#[tokio::test]
async fn test_matching_payload_restores_skipped_message_when_cancelled() -> Result<()> {
    let node = crate::tests::default::prepare_node(SecretKey::random()).await;
    let skipped = test_payload(&node, b"cancelled")?;
    node.prepend_messages_for_test(vec![skipped.clone()]).await;

    let result = timeout(
        Duration::from_millis(20),
        next_payload_matching_with_timeout(
            &node,
            "cancelled observation",
            Duration::from_secs(1),
            |_| Ok(false),
        ),
    )
    .await;

    assert!(result.is_err());
    assert_eq!(node.try_listen_once().await, Some(skipped));
    Ok(())
}

#[tokio::test]
async fn test_matching_payload_restores_current_message_when_predicate_panics() -> Result<()> {
    let node = crate::tests::default::prepare_node(SecretKey::random()).await;
    let current = test_payload(&node, b"predicate panic")?;
    node.prepend_messages_for_test(vec![current.clone()]).await;

    let result = AssertUnwindSafe(next_payload_matching_with_timeout(
        &node,
        "predicate panic",
        Duration::from_millis(20),
        |_| panic!("intentional predicate panic"),
    ))
    .catch_unwind()
    .await;

    assert!(result.is_err());
    assert_eq!(node.try_listen_once().await, Some(current));
    Ok(())
}

fn test_payload(node: &Node, data: &[u8]) -> Result<MessagePayload> {
    MessagePayload::new_send(
        Message::custom(data)?,
        node.swarm.transport.message_signer(),
        node.did(),
        node.did(),
    )
}

pub(super) fn next_generated_key(keys: &mut impl Iterator<Item = SecretKey>) -> Result<SecretKey> {
    keys.next()
        .ok_or_else(|| Error::InvalidMessage("expected generated key".to_string()))
}

pub(super) fn storage_sync_report_payload(
    request: &MessagePayload,
    report: SyncEntriesWithSuccessorReport,
    signer: MessageSigner<&SessionSk>,
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
        .dht_virtual_nodes(0)
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
        let entry: Entry = crate::tests::live(topic.try_into()?);
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
