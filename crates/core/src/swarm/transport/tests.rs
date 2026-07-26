use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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
use crate::message::MessagePayload;
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

#[derive(Default)]
struct CountingSwarmCallback {
    validates: AtomicUsize,
    inbounds: AtomicUsize,
}

impl CountingSwarmCallback {
    fn validates(&self) -> usize {
        self.validates.load(Ordering::SeqCst)
    }

    fn inbounds(&self) -> usize {
        self.inbounds.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SwarmCallback for CountingSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.validates.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        self.inbounds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

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
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].attempt, attempt);
    assert_eq!(expired[0].age_ms, PENDING_CONNECTION_TIMEOUT_MS);
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
async fn data_channel_open_admits_successor_before_ice_connected() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    let connection = transport
        .get_raw_connection(peer)
        .ok_or(Error::SwarmMissTransport(peer))?;

    connection
        .connection
        .webrtc_answer_offer("remote-dummy-connection".to_string())
        .await
        .map_err(Error::Transport)?;
    assert_eq!(
        connection.webrtc_connection_state(),
        WebrtcConnectionState::Connecting
    );
    assert!(connection.connection.data_channel_is_open()?);

    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection(peer));
    assert!(transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[tokio::test]
async fn pending_callback_messages_do_not_dispatch_before_admission() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let pending_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    let payload = MessagePayload::new_send(
        Message::custom(b"message-before-admission")?,
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?;
    let bytes = payload.to_bincode()?;

    pending_callback
        .on_message(&peer.to_string(), &bytes)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(app_callback.validates(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&peer)?);

    pending_callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    pending_callback
        .on_message(&peer.to_string(), &bytes)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    assert_eq!(measure.snapshot_counters()?.as_slice(), &[
        (peer, MeasureCounter::Connect),
        (peer, MeasureCounter::Received),
    ]);
    assert!(transport.is_admitted_connection(peer));

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

#[tokio::test]
async fn late_terminal_callback_cannot_remove_replacement_active_slot() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let old_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.retire_pending_connection(old_attempt)?);
    let current_attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.promote_pending_connection(current_attempt)?);
    let late_callback =
        InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
            .with_pending_connection_attempt(old_attempt);

    late_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection_attempt(current_attempt));
    assert_eq!(transport.admitted_connection_ids(), vec![peer]);
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

    assert_eq!(measure.snapshot_counters()?.as_slice(), &[
        (peer, MeasureCounter::Disconnected),
        (peer, MeasureCounter::Connect),
        (peer, MeasureCounter::Disconnected),
    ]);

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
