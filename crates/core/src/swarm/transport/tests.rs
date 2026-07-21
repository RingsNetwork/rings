use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_transport::core::callback::TransportCallback;

use super::*;
use crate::dht::successor::SuccessorReader;
use crate::dht::VirtualNodeConfig;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::ecc::SecretKey;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::storage::MemStorage;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
use crate::swarm::SwarmBuilder;

#[derive(Default)]
struct RecordingMeasure {
    counters: Mutex<Vec<(Did, MeasureCounter)>>,
    qualities: Mutex<BTreeMap<Did, PeerQuality>>,
}

impl RecordingMeasure {
    fn snapshot_counters(&self) -> std::io::Result<Vec<(Did, MeasureCounter)>> {
        self.counters
            .lock()
            .map(|counters| counters.clone())
            .map_err(|_| std::io::Error::other("counters poisoned"))
    }

    fn set_quality(&self, did: Did, quality: PeerQuality) -> std::io::Result<()> {
        self.qualities
            .lock()
            .map(|mut qualities| {
                qualities.insert(did, quality);
            })
            .map_err(|_| std::io::Error::other("qualities poisoned"))
    }
}

#[async_trait]
impl Measure for RecordingMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        match self.counters.lock() {
            Ok(mut counters) => counters.push((did, counter)),
            Err(_) => tracing::error!("RecordingMeasure counters mutex is poisoned"),
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
                tracing::error!("RecordingMeasure counters mutex is poisoned");
                0
            }
        }
    }
}

#[async_trait]
impl BehaviourJudgement for RecordingMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        match self.qualities.lock() {
            Ok(qualities) => qualities.get(&did).copied().unwrap_or(PeerQuality::Unknown),
            Err(_) => {
                tracing::error!("RecordingMeasure qualities mutex is poisoned");
                PeerQuality::Unknown
            }
        }
    }

    async fn good(&self, _did: Did) -> bool {
        true
    }
}

struct NoopSwarmCallback;

#[async_trait]
impl SwarmCallback for NoopSwarmCallback {}

fn transport_with_measure(measure: MeasureImpl) -> Result<SwarmTransport> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let dht = Arc::new(PeerRing::new_with_storage_and_finger_table_size(
        session_sk.account_did(),
        3,
        Box::new(MemStorage::new()),
        DEFAULT_FINGER_TABLE_SIZE,
    ));
    Ok(SwarmTransport::new(
        0,
        SwarmWebrtcConfig::new("".to_string(), None, None),
        session_sk,
        dht,
        Some(measure),
        SwarmTransportSettings::new(
            1,
            VirtualNodeConfig::disabled(),
            ReassemblyLimits::production(),
        ),
    ))
}

#[test]
fn swarm_builder_uses_chord_virtual_node_default() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let swarm = SwarmBuilder::new(7, "", Box::new(MemStorage::new()), session_sk).build();

    assert_eq!(
        swarm.dht_virtual_nodes(),
        DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(
        swarm.dht().storage_virtual_positions(swarm.did())?.len(),
        usize::from(DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER)
    );

    Ok(())
}

#[test]
fn swarm_builder_normalizes_virtual_nodes_before_protocol_advertisement() -> Result<()> {
    let key = SecretKey::random();
    let session_sk = SessionSk::new_with_seckey(&key)?;
    let requested = MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER.saturating_add(1);
    let swarm = SwarmBuilder::new(7, "", Box::new(MemStorage::new()), session_sk)
        .dht_virtual_nodes(requested)
        .build();

    assert_eq!(
        swarm.dht_virtual_nodes(),
        MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER
    );
    assert_eq!(
        swarm.dht().storage_virtual_positions(swarm.did())?.len(),
        usize::from(MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER)
    );

    Ok(())
}

#[test]
fn pending_peer_pool_is_bounded_and_rejects_duplicate_peers() -> Result<()> {
    let mut pool = PendingPeerPool::<2>::new();
    let now = 1_000;
    let peer_a = SecretKey::random().address().into();
    let peer_b = SecretKey::random().address().into();
    let peer_c = SecretKey::random().address().into();

    let attempt_a = pool.reserve(peer_a, now)?;
    assert!(matches!(
        pool.reserve(peer_a, now),
        Err(Error::AlreadyConnected)
    ));
    let _attempt_b = pool.reserve(peer_b, now)?;
    assert!(matches!(
        pool.reserve(peer_c, now),
        Err(Error::PendingConnectionCapacityExceeded { capacity: 2 })
    ));
    assert_eq!(pool.len(), 2);

    assert!(pool.remove(attempt_a));
    assert_eq!(pool.len(), 1);
    assert!(pool.reserve(peer_c, now).is_ok());
    Ok(())
}

#[test]
fn stale_pending_callback_cannot_remove_a_replacement_attempt() -> Result<()> {
    let mut pool = PendingPeerPool::<1>::new();
    let now = 1_000;
    let peer = SecretKey::random().address().into();

    let old_attempt = pool.reserve(peer, now)?;
    assert!(pool.remove(old_attempt));
    let current_attempt = pool.reserve(peer, now)?;

    assert!(!pool.remove(old_attempt));
    assert!(pool.contains(peer));
    assert!(pool.remove(current_attempt));
    Ok(())
}

#[test]
fn pending_peer_pool_expires_unopened_handshakes() -> Result<()> {
    let mut pool = PendingPeerPool::<1>::new();
    let now = 1_000;
    let peer = SecretKey::random().address().into();
    let attempt = pool.reserve(peer, now)?;

    let expired = pool.expire(now + PENDING_CONNECTION_TIMEOUT_MS);
    assert_eq!(expired, vec![attempt]);
    assert_eq!(pool.len(), 0);
    Ok(())
}

#[tokio::test]
async fn admitted_peer_cannot_be_replaced_by_a_pending_handshake() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;

    assert!(transport.promote_pending_connection(attempt)?);
    assert!(matches!(
        transport.reserve_pending_connection(peer).await,
        Err(Error::AlreadyConnected)
    ));
    Ok(())
}

#[tokio::test]
async fn pending_offer_is_not_routable_or_visible_to_dht() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));

    let _offer = transport.prepare_connection_offer(peer, callback).await?;

    assert!(transport.get_connection(peer).is_none());
    assert_eq!(transport.pending_connection_count()?, 1);
    assert!(!transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn local_did_lifecycle_callbacks_are_ignored() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let local_cid = transport.dht.did.to_string();

    callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_peer_connection_state_change(&local_cid, WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    callback
        .on_data_channel_close(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[tokio::test]
async fn mismatched_pending_callback_cancels_attempt_without_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    let mismatched_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(attempt);
    let local_cid = transport.dht.did.to_string();

    mismatched_callback
        .on_data_channel_open(&local_cid)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 0);
    assert!(transport.get_connection(peer).is_none());
    assert!(transport.get_connection(transport.dht.did).is_none());
    Ok(())
}

#[test]
fn connection_offer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let matching = ConnectNodeSend {
        sdp: String::new(),
        network_id: 0,
        storage_redundancy: 1,
        dht_virtual_nodes: 0,
    };
    let mismatched_redundancy = ConnectNodeSend {
        storage_redundancy: 2,
        ..matching.clone()
    };

    assert!(transport.accepts_connection_offer(&matching));
    assert!(!transport.accepts_connection_offer(&mismatched_redundancy));
    Ok(())
}

#[test]
fn connection_answer_protocol_mode_includes_storage_redundancy() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let matching = ConnectNodeReport {
        sdp: String::new(),
        network_id: 0,
        storage_redundancy: 1,
        dht_virtual_nodes: 0,
    };
    let mismatched_redundancy = ConnectNodeReport {
        storage_redundancy: 2,
        ..matching.clone()
    };

    assert!(transport.accepts_connection_answer(&matching));
    assert!(!transport.accepts_connection_answer(&mismatched_redundancy));
    Ok(())
}

#[tokio::test]
async fn disconnected_observation_is_once_per_connection_epoch() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = transport_with_measure(measure.clone())?;
    let peer = SecretKey::random().address().into();

    transport.record_peer_disconnected(peer).await;
    transport.record_peer_disconnected(peer).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());
    transport.record_peer_connected(peer).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_none());
    transport.record_peer_disconnected(peer).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());

    assert_eq!(
        measure.snapshot_counters()?.as_slice(),
        &[
            (peer, MeasureCounter::Disconnected),
            (peer, MeasureCounter::Connect),
            (peer, MeasureCounter::Disconnected),
        ]
    );

    Ok(())
}

#[tokio::test]
async fn dht_candidate_order_uses_peer_quality_without_dropping_candidates() -> Result<()> {
    let degraded = SecretKey::random().address().into();
    let unknown = SecretKey::random().address().into();
    let healthy = SecretKey::random().address().into();
    let measure = Arc::new(RecordingMeasure::default());
    measure.set_quality(degraded, PeerQuality::Degraded)?;
    measure.set_quality(healthy, PeerQuality::Healthy)?;
    let transport = transport_with_measure(measure)?;

    let ordered = transport
        .order_dht_candidates_by_quality([degraded, unknown, healthy])
        .await;

    assert_eq!(ordered, vec![healthy, unknown, degraded]);

    Ok(())
}
