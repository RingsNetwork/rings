use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_transport::connections::dummy_controlled;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_transport::core::transport::WebrtcConnectionState;
#[cfg(not(target_family = "wasm"))]
use tokio::time::timeout;
#[cfg(not(target_family = "wasm"))]
use tokio::time::Duration;

use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Did;
use crate::dht::PeerRingAction;
use crate::dht::StorageRepairOutcome;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::measure::PeerQuality;
use crate::measure::PeerQualityThresholds;
use crate::session::SessionSk;
use crate::storage::MemStorage;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::PEER_LIVENESS_TIMEOUT_MS;
use crate::swarm::SwarmBuilder;
use crate::tests::default::assert_no_more_msg;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::default::dummy_hooks::PendingSendGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_with_measure;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::manually_establish_connection;
use crate::tests::replace_observed_fingers;
use crate::utils::get_epoch_ms_i64;

mod test_storage_repair;

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
        crate::measure::PeerMeasurement::from_measure(self, did)
            .await
            .map(|measurement| {
                measurement
                    .evidence
                    .classify(PeerQualityThresholds::new(3, 10, 10))
            })
            .unwrap_or(PeerQuality::Unknown)
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

fn prepare_repair_node(key: SecretKey) -> Result<Node> {
    prepare_repair_node_with_optional_measure(key, None)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn prepare_repair_node_with_measure(key: SecretKey, measure: MeasureImpl) -> Result<Node> {
    prepare_repair_node_with_optional_measure(key, Some(measure))
}

fn prepare_repair_node_with_optional_measure(
    key: SecretKey,
    measure: Option<MeasureImpl>,
) -> Result<Node> {
    let session = SessionSk::new_with_seckey(&key)?;
    let mut builder = SwarmBuilder::new(
        0,
        "stun://stun.l.google.com:19302",
        Box::new(MemStorage::new()),
        session,
    )
    .dht_finger_table_size(super::TEST_DHT_FINGER_TABLE_SIZE)
    .dht_storage_redundancy(2)
    .dht_virtual_nodes(0);
    if let Some(measure) = measure {
        builder = builder.measure(measure);
    }
    let swarm = Arc::new(builder.build());
    Ok(Node::new(swarm))
}

fn repair_test_keys() -> Result<(SecretKey, SecretKey)> {
    let mut first =
        SecretKey::try_from("65860affb4b570dba06db294aa7c676f68e04a5bf2721243ad3cbc05a79c68c0")?;
    let mut second =
        SecretKey::try_from("1f9275dbafdfba81942eb3330b07f38cbee4ebb86bdc2174af9648d5f5509a54")?;
    if first.address() < second.address() {
        (first, second) = (second, first);
    }
    Ok((first, second))
}

fn entry_for_remote_repair_placement(node: &Node, successor: Did) -> Result<(Entry, Did)> {
    // The first key clockwise after the known successor is outside the local successor interval,
    // hence requires the remote continuation branch. Fixed node identities make this witness
    // deterministic; there is no probabilistic hash search in the test precondition.
    let placement = successor + Did::from(1_u32);
    if matches!(
        node.dht().find_storage_owner(placement)?,
        PeerRingAction::RemoteAction(_, _)
    ) {
        // `rotate_affine(n)[0] = self`, so choosing the witnessed placement as the entry key
        // deterministically makes it one of the repair placements for every non-zero redundancy.
        return Ok((Entry::new(placement, vec![], EntryKind::Data), placement));
    }
    Err(Error::InvalidMessage(
        "remote repair fixture DID did not route remotely".to_string(),
    ))
}

#[cfg(not(target_family = "wasm"))]
pub(super) fn replace_observed_topology(
    node: &Node,
    successors: &[Did],
    predecessor: Option<Did>,
    fingers: &[(usize, Did)],
) -> Result<()> {
    let successor_seq = node.dht().successors();
    for did in successor_seq.list()? {
        successor_seq.remove(did)?;
    }
    successor_seq.extend(successors)?;
    *node.dht().lock_predecessor()? = predecessor;
    replace_observed_fingers(&node.swarm, fingers)
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
async fn test_get_and_check_connection_times_out_wedged_data_channel_wait() -> Result<()> {
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
            .get_and_check_send_connection_with_timeout(node2.did(), Duration::from_millis(20)),
    )
    .await
    .map_err(|_| Error::PromiseStateTimeout)?;

    assert!(conn.is_none());
    assert!(!node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(!node1.dht().successors().contains(&node2.did())?);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_get_and_check_connection_waits_for_disconnected_open_transport() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node2.did(),
            WebrtcConnectionState::Disconnected,
        )?;
    node1
        .swarm
        .transport
        .force_peer_data_channel_open_without_callback(node2.did(), Some(true))?;

    let conn = timeout(
        Duration::from_secs(1),
        node1
            .swarm
            .transport
            .get_and_check_send_connection_with_timeout(node2.did(), Duration::from_millis(20)),
    )
    .await
    .map_err(|_| Error::PromiseStateTimeout)?;

    assert!(conn.is_none());
    assert!(!node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(!node1.dht().successors().contains(&node2.did())?);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_liveness_probe_backpressure_does_not_degrade_peer() -> Result<()> {
    let measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = measure.clone();
    let node1 = prepare_node_with_measure(SecretKey::random(), measure_impl)?;
    let node2 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;
    node1
        .swarm
        .transport
        .force_peer_connected_at(node2.did(), get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1)?;

    let _pending_send = PendingSendGuard::new();
    node1
        .swarm
        .stabilizer()
        .stabilize_with_step_timeout(Duration::from_secs(1))
        .await?;

    assert_eq!(
        measure
            .get_count(node2.did(), MeasureCounter::FailedToSend)
            .await,
        0
    );
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_removes_silent_connected_peer() -> Result<()> {
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

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_observes_disconnected_peer_without_callback(
) -> Result<()> {
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

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node2.did(),
            WebrtcConnectionState::Disconnected,
        )?;

    assert_eq!(
        node1
            .swarm
            .transport
            .peer_disconnected_since_ms(node2.did()),
        None
    );

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
    assert!(node1.dht().lock_finger()?.contains(Some(node2.did())));
    assert!(node1
        .swarm
        .transport
        .peer_disconnected_since_ms(node2.did())
        .is_some());

    node1
        .swarm
        .transport
        .force_peer_disconnected_since_ms(node2.did(), get_epoch_ms_i64().saturating_sub(60_000))?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(!node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(!node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_fails_over_to_live_successor_tail() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let node3 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    replace_observed_topology(&node1, &[node2.did(), node3.did()], None, &[])?;
    let successors = node1.dht().successors().list()?;
    assert_eq!(successors.len(), 2);
    let disconnected_head = successors[0];
    let live_tail = successors[1];

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            disconnected_head,
            WebrtcConnectionState::Disconnected,
        )?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(!node1
        .swarm
        .transport
        .is_admitted_connection(disconnected_head));
    assert!(node1
        .swarm
        .transport
        .get_connection(disconnected_head)
        .is_none());
    assert!(!node1.dht().successors().contains(&disconnected_head)?);
    assert!(node1.dht().successors().contains(&live_tail)?);
    assert_eq!(
        node1.dht().successors().get(0)?,
        live_tail,
        "successor tail must become the new head"
    );
    assert_eq!(
        node1
            .swarm
            .transport
            .get_connection(live_tail)
            .map(|conn| conn.webrtc_connection_state()),
        Some(WebrtcConnectionState::Connected)
    );

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_prunes_disconnected_non_head_slots() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let node3 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    replace_observed_topology(&node1, &[node2.did(), node3.did()], None, &[])?;
    let successors = node1.dht().successors().list()?;
    assert_eq!(successors.len(), 2);
    let live_head = successors[0];
    let disconnected_tail = successors[1];
    replace_observed_topology(
        &node1,
        &[live_head, disconnected_tail],
        Some(disconnected_tail),
        &[(0, disconnected_tail), (3, disconnected_tail)],
    )?;

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            disconnected_tail,
            WebrtcConnectionState::Disconnected,
        )?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.is_admitted_connection(live_head));
    assert!(node1
        .swarm
        .transport
        .is_admitted_connection(disconnected_tail));
    assert!(node1.dht().successors().contains(&live_head)?);
    assert!(!node1.dht().successors().contains(&disconnected_tail)?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(disconnected_tail)));
    assert_eq!(
        node1
            .swarm
            .transport
            .admitted_connection(disconnected_tail)?
            .map(|conn| conn.webrtc_connection_state()),
        Some(WebrtcConnectionState::Disconnected),
        "topology prune must not close a transiently disconnected transport"
    );
    assert!(node1
        .swarm
        .transport
        .get_connection(disconnected_tail)
        .is_none());

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_does_not_fail_over_to_disconnected_finger() -> Result<()>
{
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;
    let node3 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    manually_establish_connection(&node1.swarm, &node3.swarm).await;
    wait_for_msgs([&node1, &node2, &node3]).await;

    replace_observed_topology(&node1, &[node2.did()], Some(node2.did()), &[
        (0, node3.did()),
        (3, node3.did()),
    ])?;

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node2.did(),
            WebrtcConnectionState::Disconnected,
        )?;
    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node3.did(),
            WebrtcConnectionState::Disconnected,
        )?;
    node1
        .swarm
        .transport
        .force_peer_data_channel_open_without_callback(node3.did(), Some(true))?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert!(!node1.dht().successors().contains(&node3.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
    assert!(!node1.dht().lock_finger()?.contains(Some(node3.did())));
    assert_eq!(
        node1
            .swarm
            .transport
            .admitted_connection(node2.did())?
            .map(|conn| conn.webrtc_connection_state()),
        Some(WebrtcConnectionState::Disconnected),
        "head successor must wait for grace when every fallback is also bad"
    );
    assert_eq!(
        node1
            .swarm
            .transport
            .admitted_connection(node3.did())?
            .map(|conn| conn.webrtc_connection_state()),
        Some(WebrtcConnectionState::Disconnected),
        "bad finger is pruned from topology before transport grace closes it"
    );
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(node1.swarm.transport.get_connection(node3.did()).is_none());

    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_prunes_disconnected_finger() -> Result<()> {
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_msgs([&node1, &node2]).await;

    replace_observed_topology(&node1, &[], None, &[(0, node2.did()), (3, node2.did())])?;

    node1
        .swarm
        .transport
        .force_peer_connection_state_without_callback(
            node2.did(),
            WebrtcConnectionState::Disconnected,
        )?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(node1.swarm.transport.is_admitted_connection(node2.did()));
    assert_eq!(
        node1
            .swarm
            .transport
            .admitted_connection(node2.did())?
            .map(|conn| conn.webrtc_connection_state()),
        Some(WebrtcConnectionState::Disconnected)
    );
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());
    assert!(!node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, None);
    assert!(!node1.dht().lock_finger()?.contains(Some(node2.did())));
    assert!(node1
        .swarm
        .transport
        .peer_disconnected_since_ms(node2.did())
        .is_some());

    node1
        .swarm
        .transport
        .force_peer_disconnected_since_ms(node2.did(), get_epoch_ms_i64().saturating_sub(60_000))?;

    node1
        .swarm
        .stabilizer()
        .clean_unavailable_connections()
        .await?;

    assert!(!node1.swarm.transport.is_admitted_connection(node2.did()));
    assert!(node1.swarm.transport.get_connection(node2.did()).is_none());

    Ok(())
}

#[tokio::test]
async fn test_clean_unavailable_connections_removes_stale_topology_peer() -> Result<()> {
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
async fn test_clean_unavailable_connections_keeps_degraded_admitted_peer() -> Result<()> {
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

    assert!(node1.swarm.transport.get_connection(node2.did()).is_some());
    assert!(node1.dht().successors().contains(&node2.did())?);
    assert_eq!(*node1.dht().lock_predecessor()?, Some(node2.did()));
    assert!(node1.dht().lock_finger()?.contains(Some(node2.did())));

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
