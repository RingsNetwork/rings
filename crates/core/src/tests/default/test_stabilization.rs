use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_transport::connections::dummy_controlled;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use tokio::time::timeout;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::measure::PeerQuality;
use crate::measure::PeerQualityEvidence;
use crate::measure::PeerQualityThresholds;
use crate::session::SessionSk;
use crate::storage::MemStorage;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::PEER_LIVENESS_TIMEOUT_MS;
use crate::swarm::SwarmBuilder;
use crate::tests::default::assert_no_more_msg;
use crate::tests::default::prepare_node;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;
use crate::utils::get_epoch_ms_i64;

#[derive(Default)]
struct CountingMeasure {
    counters: Mutex<Vec<(Did, MeasureCounter)>>,
}

#[async_trait]
impl Measure for CountingMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        match self.counters.lock() {
            Ok(mut counters) => counters.push((did, counter)),
            Err(_) => tracing::error!("CountingMeasure counters mutex is poisoned"),
        }
    }

    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        match self.counters.lock() {
            Ok(counters) => counters
                .iter()
                .filter(|(observed_did, observed_counter)| {
                    *observed_did == did && *observed_counter == counter
                })
                .count() as u64,
            Err(_) => {
                tracing::error!("CountingMeasure counters mutex is poisoned");
                0
            }
        }
    }
}

#[async_trait]
impl BehaviourJudgement for CountingMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        PeerQualityEvidence::from_measure(self, did)
            .await
            .classify(PeerQualityThresholds::new(3, 10, 10))
    }

    async fn good(&self, did: Did) -> bool {
        self.quality(did).await != PeerQuality::Degraded
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
struct PendingDataChannelWaitGuard;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl PendingDataChannelWaitGuard {
    fn new() -> Self {
        dummy_controlled::set_wait_for_data_channel_open_pending(true);
        Self
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl Drop for PendingDataChannelWaitGuard {
    fn drop(&mut self) {
        dummy_controlled::set_wait_for_data_channel_open_pending(false);
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
struct DropMessagesGuard;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl DropMessagesGuard {
    fn new() -> Self {
        dummy_controlled::set_drop_messages(true);
        Self
    }
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
impl Drop for DropMessagesGuard {
    fn drop(&mut self) {
        dummy_controlled::set_drop_messages(false);
    }
}

fn prepare_node_with_measure(key: SecretKey, measure: MeasureImpl) -> Result<Node> {
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_finger_table_size(super::TEST_DHT_FINGER_TABLE_SIZE)
        .dht_virtual_nodes(0)
        .measure(measure)
        .build(),
    );
    Ok(Node::new(swarm))
}

fn prepare_repair_node(key: SecretKey) -> Result<Node> {
    let session = SessionSk::new_with_seckey(&key)?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            "stun://stun.l.google.com:19302",
            Box::new(MemStorage::new()),
            session,
        )
        .dht_finger_table_size(super::TEST_DHT_FINGER_TABLE_SIZE)
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0)
        .build(),
    );
    Ok(Node::new(swarm))
}

fn entry_with_remote_repair_placement(node: &Node) -> Result<(Entry, Did)> {
    for attempt in 0..1024 {
        let resource = Entry::gen_did(&format!("fresh repair candidate {attempt}"))?;
        let entry = Entry::new(resource, vec![], EntryKind::Data);
        for placement in entry.did.rotate_affine(2)? {
            if matches!(
                node.dht().find_storage_owner(placement)?,
                PeerRingAction::RemoteAction(_, _)
            ) {
                return Ok((entry, placement));
            }
        }
    }

    Err(Error::InvalidMessage(
        "could not sample remote repair placement".to_string(),
    ))
}

#[tokio::test]
async fn test_stabilization_once() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    println!("swarm1: {:?}, swarm2: {:?}", node1.did(), node2.did());

    wait_for_successor(&node1, key2.address().into()).await?;
    wait_for_successor(&node2, key1.address().into()).await?;

    let stabilizer = node1.swarm.stabilizer();
    stabilizer.stabilize().await?;
    wait_for_predecessor(&node2, key1.address().into()).await?;
    wait_for_successor(&node1, key2.address().into()).await?;

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn get_and_check_connection_times_out_wedged_data_channel_wait() -> Result<()> {
    let key1 = SecretKey::random();
    let key2 = SecretKey::random();
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;

    let _guard = PendingDataChannelWaitGuard::new();
    let conn = timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .transport
            .get_and_check_connection_with_timeout(node2.did(), Duration::from_millis(20)),
    )
    .await
    .map_err(|_| Error::PromiseStateTimeout)?;

    assert!(conn.is_none());
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn stabilize_step_timeout_bounds_wedged_data_channel_wait() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;

    let _guard = PendingDataChannelWaitGuard::new();
    timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .stabilizer()
            .stabilize_with_step_timeout(Duration::from_millis(20)),
    )
    .await
    .map_err(|_| Error::PromiseStateTimeout)??;

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn clean_unavailable_connections_removes_silent_connected_peer() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;

    node1.dht().successors().extend(&[node2.did()])?;
    *node1.dht().lock_predecessor()? = Some(node2.did());
    {
        let dht = node1.dht();
        let mut finger = dht.lock_finger()?;
        finger.set(0, node2.did());
        finger.set(3, node2.did());
    }

    let stale_probe_sent_at = get_epoch_ms_i64() - PEER_LIVENESS_TIMEOUT_MS - 1;
    node1
        .swarm
        .transport
        .force_peer_liveness_probe_sent_at(node2.did(), stale_probe_sent_at)?;

    let _drop_messages = DropMessagesGuard::new();
    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(!node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));

    Ok(())
}

#[tokio::test]
async fn clean_unavailable_connections_removes_stale_topology_peer() -> Result<()> {
    let node = prepare_node(SecretKey::random()).await;
    let stale = SecretKey::random().address().into();

    node.dht().successors().extend(&[stale])?;
    *node.dht().lock_predecessor()? = Some(stale);
    {
        let dht = node.dht();
        let mut finger = dht.lock_finger()?;
        finger.set(0, stale);
        finger.set(3, stale);
    }

    assert!(node.dht().successors().contains(&stale)?);
    assert_eq!(*node.dht().lock_predecessor()?, Some(stale));
    assert!(node.dht().lock_finger()?.contains(Some(stale)));
    assert!(!node.swarm.transport.is_admitted_connection(stale));

    node.swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(!node.dht().successors().contains(&stale)?);
    assert_eq!(*node.dht().lock_predecessor()?, None);
    assert!(!node.dht().lock_finger()?.contains(Some(stale)));

    Ok(())
}

#[tokio::test]
async fn clean_unavailable_connections_removes_degraded_admitted_peer() -> Result<()> {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;

    node1.dht().successors().extend(&[node2.did()])?;
    *node1.dht().lock_predecessor()? = Some(node2.did());
    {
        let dht = node1.dht();
        let mut finger = dht.lock_finger()?;
        finger.set(0, node2.did());
        finger.set(3, node2.did());
    }

    for _ in 0..10 {
        node1
            .swarm
            .transport
            .record_peer_message_send_failed(node2.did())
            .await;
    }
    assert_eq!(
        measure
            .get_count(node2.did(), MeasureCounter::FailedToSend)
            .await,
        10
    );

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(!node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));

    Ok(())
}

#[tokio::test]
async fn stabilize_republishes_local_entries_to_missing_affine_owners() -> Result<()> {
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
    let entry = Entry::new(key.address().into(), vec![], EntryKind::Data);
    let placement_keys = entry.did.rotate_affine(2)?;
    node.dht()
        .storage
        .put(&placement_keys[0].to_string(), &entry)
        .await?;

    node.swarm.stabilizer().stabilize().await?;

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
async fn repair_storage_defers_sync_to_fresh_next_hop() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_repair_node(key1)?;
    let node2 = prepare_repair_node(key2)?;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_msgs([&node1, &node2]).await;
    let connected_for_ms = node1
        .swarm
        .transport
        .peer_connected_for_ms(node2.did(), get_epoch_ms_i64())?
        .ok_or_else(|| Error::InvalidMessage("missing peer admission age".to_string()))?;
    assert!(
        connected_for_ms < 30_000,
        "test must exercise a fresh connection; observed age {connected_for_ms}ms"
    );

    let (entry, remote_placement) = entry_with_remote_repair_placement(&node1)?;
    node1
        .dht()
        .storage
        .put(&entry.did.to_string(), &entry)
        .await?;

    node1.swarm.stabilizer().repair_storage().await?;

    assert_no_more_msg([&node2]).await;
    assert_eq!(
        node2
            .dht()
            .storage
            .get(&remote_placement.to_string())
            .await?,
        None
    );
    Ok(())
}

#[tokio::test]
async fn test_stabilization() -> Result<()> {
    let mut key1 = SecretKey::random();
    let mut key2 = SecretKey::random();
    // key 2 > key 1 here
    if key1.address() < key2.address() {
        (key1, key2) = (key2, key1)
    }
    let node1 = prepare_node(key1).await;
    let node2 = prepare_node(key2).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;

    wait_for_successor(&node1, key2.address().into()).await?;
    wait_for_successor(&node2, key1.address().into()).await?;

    let stabilizer1 = node1.swarm.stabilizer();
    let stabilizer2 = node2.swarm.stabilizer();
    tokio::try_join!(stabilizer1.stabilize(), stabilizer2.stabilize())?;

    wait_for_predecessor(&node2, key1.address().into()).await?;
    wait_for_predecessor(&node1, key2.address().into()).await?;
    Ok(())
}
