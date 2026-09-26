use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_measure::EvidenceLimits;
use rings_measure::ProvisionalEvidenceStore;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_transport::connections::dummy_controlled;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use rings_transport::core::transport::WebrtcConnectionState;
#[cfg(not(target_family = "wasm"))]
use tokio::time::timeout;
#[cfg(not(target_family = "wasm"))]
use tokio::time::Duration;

use crate::delegation::DelegateeKey;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryKind;
use crate::dht::Did;
use crate::dht::EntryStorage;
use crate::dht::PeerRingAction;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::dht::StorageRepairOutcome;
use crate::ecc::SecretKey;
use crate::error::Error;
use crate::error::Result;
use crate::measure::BehaviourJudgement;
use crate::measure::EvidenceAdmissionReport;
use crate::measure::EvidenceCounters;
use crate::measure::EvidenceDigest;
use crate::measure::EvidenceError;
use crate::measure::EvidencePage;
use crate::measure::Measure;
use crate::measure::MeasureImpl;
use crate::measure::MeasurementEvent;
use crate::measure::PeerQuality;
use crate::measure::PeerQualityThresholds;
use crate::measure::ProvisionalEvidenceRecord;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::ProvisionalServiceReceipt;
use crate::storage::MemStorage;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::transport::PEER_LIVENESS_TIMEOUT_MS;
use crate::swarm::SwarmBuilder;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::default::assert_no_more_msg;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::tests::default::dummy_hooks::PendingSendGuard;
use crate::tests::default::prepare_node;
use crate::tests::default::prepare_node_with_measure;
use crate::tests::default::wait_for_msgs;
use crate::tests::default::wait_for_predecessor;
use crate::tests::default::wait_for_successor;
use crate::tests::default::Node;
use crate::tests::fixed_secret_keys;
use crate::tests::live_entry;
use crate::tests::manually_establish_connection;
use crate::tests::replace_observed_fingers;
use crate::utils::get_epoch_ms_i64;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_storage_handoff;
mod test_storage_repair;

struct CountingMeasure {
    counters: Mutex<Vec<(Did, MeasurementEvent)>>,
    evidence: Mutex<ProvisionalEvidenceStore<Did>>,
}

impl Default for CountingMeasure {
    fn default() -> Self {
        Self {
            counters: Mutex::new(Vec::new()),
            evidence: Mutex::new(ProvisionalEvidenceStore::new(EvidenceLimits::default())),
        }
    }
}

#[async_trait]
impl Measure for CountingMeasure {
    async fn admit_provisional_evidence(
        &self,
        record: ProvisionalEvidenceRecord<Did>,
    ) -> std::result::Result<EvidenceAdmissionReport<Did>, EvidenceError> {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .admit(record)
    }

    async fn provisional_evidence_page(
        &self,
        after: Option<EvidenceDigest>,
        limit: NonZeroUsize,
    ) -> std::result::Result<EvidencePage<Did>, EvidenceError> {
        Ok(self
            .evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .page(after, limit))
    }

    async fn provisional_evidence_counters(&self) -> EvidenceCounters {
        self.evidence
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .counters()
    }
    /// Apply one explicitly attributed observation in this test double.
    async fn record(
        &self,
        did: crate::dht::Did,
        authentication: crate::measure::Authentication,
        event: MeasurementEvent,
    ) -> std::result::Result<crate::measure::ApplyOutcome, crate::measure::MeasureError> {
        if !authentication.permits(event) {
            return Ok(crate::measure::ApplyOutcome::IgnoredUnattributable);
        }
        self.observe_event(did, event).await;
        Ok(crate::measure::ApplyOutcome::Applied)
    }

    /// Record all permitted occurrences together in the test event log.
    async fn record_batch(
        &self,
        did: crate::dht::Did,
        authentication: crate::measure::Authentication,
        batch: crate::measure::MeasurementBatch,
    ) -> std::result::Result<crate::measure::ApplyOutcome, crate::measure::MeasureError> {
        if !authentication.permits(batch.event()) {
            return Ok(crate::measure::ApplyOutcome::IgnoredUnattributable);
        }
        // A single lock publishes every occurrence in this test batch together.
        self.counters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend((0..batch.occurrences().get()).map(|_occurrence| (did, batch.event())));
        Ok(crate::measure::ApplyOutcome::Applied)
    }
}
impl CountingMeasure {
    /// Test-only event observation, independent of the runtime Measure API.
    async fn observe_event(&self, did: Did, counter: MeasurementEvent) {
        match self.counters.lock() {
            Ok(mut counters) => counters.push((did, counter)),
            Err(_) => tracing::error!("CountingMeasure counters mutex is poisoned"),
        }
    }
    /// Count event kinds without discarding useful bytes from the recorded events.
    async fn event_count(&self, did: Did, counter: MeasurementEvent) -> u64 {
        match self.counters.lock() {
            Ok(counters) => counters
                .iter()
                .filter(|(observed_did, observed_counter)| {
                    *observed_did == did
                        && std::mem::discriminant(observed_counter)
                            == std::mem::discriminant(&counter)
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
        // This test fixture chooses a complete policy and queries its recorded events.
        let evidence = crate::measure::PeerQualityEvidence::new(
            self.event_count(did, MeasurementEvent::Connected).await,
            self.event_count(did, MeasurementEvent::Disconnected).await,
            self.event_count(did, MeasurementEvent::Sent { useful_bytes: 0 })
                .await,
            self.event_count(did, MeasurementEvent::FailedToSend).await,
            self.event_count(did, MeasurementEvent::Received { useful_bytes: 0 })
                .await,
            self.event_count(did, MeasurementEvent::FailedToReceive)
                .await,
        );
        // The explicit window is immaterial for this fixture's untimed event log.
        let policy =
            rings_measure::ReliabilityPolicy::new(60, 1, PeerQualityThresholds::new(3, 10, 10))
                .unwrap();
        evidence.classify_with_policy(policy)
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
    prepare_repair_node_with_storage(key, Box::new(MemStorage::new()), measure)
}

/// Build a repair-test node over the given DHT entry storage, with host-only ICE.
fn prepare_repair_node_with_storage(
    key: SecretKey,
    storage: EntryStorage,
    measure: Option<MeasureImpl>,
) -> Result<Node> {
    let session = DelegateeKey::new_with_seckey(&key)?;
    let mut builder = SwarmBuilder::new(0, super::TEST_ICE_SERVERS, storage, session)
        .dht_finger_table_size(super::TEST_DHT_FINGER_TABLE_SIZE)
        .dht_storage_redundancy(2)
        .dht_virtual_nodes(0);
    if let Some(measure) = measure {
        builder = builder.measure(measure);
    }
    Ok(Node::build(builder))
}

fn repair_test_keys() -> Result<(SecretKey, SecretKey)> {
    // Descending address order: the first identity is the higher one.
    let [lower, higher] = fixed_secret_keys::<2>()?;
    Ok((higher, lower))
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
        return Ok((live_entry(placement, vec![], EntryKind::Data), placement));
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

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;

    let stabilizer = node1.swarm.stabilizer();
    stabilizer.stabilize().await?;
    wait_for_predecessor(&node2, node1.did()).await?;
    wait_for_successor(&node1, node2.did()).await?;

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

    // The send is refused; retirement belongs to stabilization, so the
    // connection and the successor slot survive one impatient sender.
    assert!(conn.is_none());
    assert!(node1.swarm.transport.has_active_connection(node2.did()));
    assert!(node1.dht().successors().contains(&node2.did())?);
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

    // The send is refused; retirement belongs to stabilization, so the
    // connection and the successor slot survive one impatient sender.
    assert!(conn.is_none());
    assert!(node1.swarm.transport.has_active_connection(node2.did()));
    assert!(node1.dht().successors().contains(&node2.did())?);
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
            .event_count(node2.did(), MeasurementEvent::FailedToSend)
            .await,
        0
    );
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_liveness_probe_round_trip_admits_provider_evidence() -> Result<()> {
    let provider_measure = Arc::new(CountingMeasure::default());
    let measure_impl: MeasureImpl = provider_measure.clone();
    let beneficiary = prepare_node(SecretKey::random()).await;
    let provider = prepare_node_with_measure(SecretKey::random(), measure_impl)?;

    manually_establish_connection(&beneficiary.swarm, &provider.swarm).await;
    wait_for_successor(&beneficiary, provider.did()).await?;
    wait_for_successor(&provider, beneficiary.did()).await?;
    wait_for_msgs([&beneficiary, &provider]).await;
    beneficiary.swarm.transport.force_peer_last_inbound_at(
        provider.did(),
        get_epoch_ms_i64() - PEER_LIVENESS_IDLE_MS - 1,
    )?;

    beneficiary
        .swarm
        .stabilizer()
        .probe_peer_liveness_for_simulation()
        .await?;
    wait_for_msgs([&beneficiary, &provider]).await;

    let page = provider_measure
        .provisional_evidence_page(None, NonZeroUsize::MIN)
        .await?;
    let receipt = page
        .records()
        .first()
        .ok_or_else(|| Error::InvalidMessage("provider admitted no probe receipt".to_string()))?;
    let decoded = ProvisionalServiceReceipt::from_canonical_bytes(receipt.canonical_receipt())?;
    let counters = provider_measure.provisional_evidence_counters().await;

    assert_eq!(page.records().len(), 1);
    assert_eq!(decoded.claim.provider_account, provider.did());
    assert_eq!(decoded.claim.beneficiary_account, beneficiary.did());
    assert_eq!(counters.admitted(), 1);
    assert_eq!(counters.duplicates(), 0);
    assert_eq!(counters.conflicts(), 0);
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

/// The cleaner also removes admitted connections no topology slot references; such a removal
/// changes no placement, so it must not request a storage repair round.
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_clean_unavailable_connections_requests_no_repair_for_unreferenced_peer() -> Result<()>
{
    let node1 = prepare_node(SecretKey::random()).await;
    let node2 = prepare_node(SecretKey::random()).await;

    manually_establish_connection(&node1.swarm, &node2.swarm).await;
    wait_for_successor(&node1, node2.did()).await?;

    // The connection stays admitted while every slot that referenced it is vacated.
    node1.dht().remove(node2.did())?;
    assert!(!node1.dht().topology_state()?.references(node2.did()));
    assert!(node1.swarm.transport.has_active_connection(node2.did()));
    node1.swarm.transport.claim_storage_repair();

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

    assert!(!node1.swarm.transport.has_active_connection(node2.did()));
    assert!(!node1.swarm.transport.storage_repair_requested());

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

    assert!(node1.swarm.transport.has_active_connection(node2.did()));
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

    assert!(!node1.swarm.transport.has_active_connection(node2.did()));
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
        .has_active_connection(disconnected_head));
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

    assert!(node1.swarm.transport.has_active_connection(live_head));
    assert!(node1
        .swarm
        .transport
        .has_active_connection(disconnected_tail));
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

    assert!(node1.swarm.transport.has_active_connection(node2.did()));
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

    assert!(node1.swarm.transport.has_active_connection(node2.did()));
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

    assert!(!node1.swarm.transport.has_active_connection(node2.did()));
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
    assert!(!node.swarm.transport.has_active_connection(stale));

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
            .record_peer_message_send_failed(
                node2.did(),
                crate::measure::Authentication::Authenticated,
            )
            .await;
    }
    assert_eq!(
        measure
            .event_count(node2.did(), MeasurementEvent::FailedToSend)
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

    wait_for_successor(&node1, node2.did()).await?;
    wait_for_successor(&node2, node1.did()).await?;

    let stabilizer1 = node1.swarm.stabilizer();
    let stabilizer2 = node2.swarm.stabilizer();
    tokio::try_join!(stabilizer1.stabilize(), stabilizer2.stabilize())?;

    wait_for_predecessor(&node2, node1.did()).await?;
    wait_for_predecessor(&node1, node2.did()).await?;
    Ok(())
}
