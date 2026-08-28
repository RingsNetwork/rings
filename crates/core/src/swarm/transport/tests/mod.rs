use std::collections::BTreeMap;
#[cfg(feature = "dummy")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "dummy")]
use std::sync::atomic::AtomicUsize;
#[cfg(feature = "dummy")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "dummy")]
use bytes::Bytes;
use rings_transport::core::callback::TransportCallback;
#[cfg(feature = "dummy")]
use tokio::sync::Notify;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use tracing_test::traced_test;

use super::pending::ConnectionLifecycleRegistry;
#[cfg(feature = "dummy")]
use super::pending::FingerUpdateDisposition;
use super::*;
#[cfg(feature = "dummy")]
use crate::chunk::Chunk;
#[cfg(feature = "dummy")]
use crate::chunk::ChunkMeta;
use crate::dht::successor::SuccessorReader;
#[cfg(feature = "dummy")]
use crate::dht::Chord;
use crate::dht::VirtualNodeConfig;
use crate::dht::DEFAULT_FINGER_TABLE_SIZE;
use crate::dht::DEFAULT_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::dht::MAX_STORAGE_VIRTUAL_POSITIONS_PER_OWNER;
use crate::ecc::SecretKey;
use crate::measure::ApplyOutcome;
use crate::measure::BehaviourJudgement;
use crate::measure::Measure;
use crate::measure::MeasureCounter;
use crate::measure::MeasureError;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::message::MessageClass;
use crate::message::MessagePayload;
use crate::storage::MemStorage;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::callback::max_on_message_recursion_depth_for_test;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
use crate::swarm::callback::reset_on_message_recursion_depth_for_test;
use crate::swarm::callback::InnerSwarmCallback;
use crate::swarm::callback::SwarmCallback;
#[cfg(feature = "dummy")]
use crate::swarm::callback::SwarmEvent;
use crate::swarm::SwarmBuilder;

mod test_events;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_finger;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_inbound;
mod test_lifecycle;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod test_readiness;
mod test_retirement;

#[derive(Default)]
struct RecordingMeasure {
    counters: Mutex<Vec<(Did, MeasureCounter)>>,
    measurements: Mutex<Vec<(Did, MeasurementEvent)>>,
    qualities: Mutex<BTreeMap<Did, PeerQuality>>,
}

impl RecordingMeasure {
    fn snapshot_counters(&self) -> std::io::Result<Vec<(Did, MeasureCounter)>> {
        self.counters
            .lock()
            .map(|counters| counters.clone())
            .map_err(|_| std::io::Error::other("counters poisoned"))
    }

    #[cfg(feature = "dummy")]
    fn snapshot_measurements(&self) -> std::io::Result<Vec<(Did, MeasurementEvent)>> {
        self.measurements
            .lock()
            .map(|measurements| measurements.clone())
            .map_err(|_| std::io::Error::other("measurements poisoned"))
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

    async fn record(
        &self,
        did: Did,
        event: MeasurementEvent,
    ) -> std::result::Result<ApplyOutcome, MeasureError> {
        match self.measurements.lock() {
            Ok(mut measurements) => measurements.push((did, event)),
            Err(_) => tracing::error!("RecordingMeasure measurements mutex is poisoned"),
        }
        self.incr(did, MeasureCounter::from_event(event)).await;
        Ok(ApplyOutcome::Applied)
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

#[cfg(feature = "dummy")]
#[derive(Default)]
struct CountingSwarmCallback {
    validates: AtomicUsize,
    inbounds: AtomicUsize,
    events: Mutex<Vec<WebrtcConnectionState>>,
}

#[cfg(feature = "dummy")]
impl CountingSwarmCallback {
    fn validates(&self) -> usize {
        self.validates.load(Ordering::SeqCst)
    }

    fn inbounds(&self) -> usize {
        self.inbounds.load(Ordering::SeqCst)
    }

    fn events(&self) -> std::io::Result<Vec<WebrtcConnectionState>> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| std::io::Error::other("events poisoned"))
    }

    fn clear_events(&self) -> std::io::Result<()> {
        self.events
            .lock()
            .map(|mut events| events.clear())
            .map_err(|_| std::io::Error::other("events poisoned"))
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl SwarmCallback for CountingSwarmCallback {
    async fn on_validate(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.validates.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_inbound(
        &self,
        _payload: &MessagePayload,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        self.inbounds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let SwarmEvent::ConnectionStateChange { state, .. } = event;
        match self.events.lock() {
            Ok(mut events) => events.push(*state),
            Err(_) => tracing::error!("CountingSwarmCallback events mutex is poisoned"),
        }
        Ok(())
    }
}

#[cfg(feature = "dummy")]
#[derive(Default)]
struct BlockingConnectMeasure {
    inner: RecordingMeasure,
    connect_started: AtomicBool,
    connect_started_notify: Notify,
    release_connect: Notify,
}

#[cfg(feature = "dummy")]
impl BlockingConnectMeasure {
    async fn wait_for_connect_started(&self) {
        while !self.connect_started.load(Ordering::SeqCst) {
            self.connect_started_notify.notified().await;
        }
    }

    fn release_connect(&self) {
        self.release_connect.notify_waiters();
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl Measure for BlockingConnectMeasure {
    async fn incr(&self, did: Did, counter: MeasureCounter) {
        if counter == MeasureCounter::Connect {
            self.connect_started.store(true, Ordering::SeqCst);
            self.connect_started_notify.notify_waiters();
            self.release_connect.notified().await;
        }
        self.inner.incr(did, counter).await;
    }

    async fn get_count(&self, did: Did, counter: MeasureCounter) -> u64 {
        self.inner.get_count(did, counter).await
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl BehaviourJudgement for BlockingConnectMeasure {
    async fn quality(&self, did: Did) -> PeerQuality {
        self.inner.quality(did).await
    }

    async fn good(&self, did: Did) -> bool {
        self.inner.good(did).await
    }
}

#[cfg(feature = "dummy")]
#[derive(Default)]
struct BlockingEventSwarmCallback {
    blocked_peer: Mutex<Option<Did>>,
    connected_started: AtomicBool,
    connected_started_notify: Notify,
    release_connected: Notify,
    events: Mutex<Vec<(Did, WebrtcConnectionState)>>,
}

#[cfg(feature = "dummy")]
impl BlockingEventSwarmCallback {
    fn blocking_peer(peer: Did) -> Self {
        Self {
            blocked_peer: Mutex::new(Some(peer)),
            ..Self::default()
        }
    }

    async fn wait_for_connected_event_started(&self) {
        while !self.connected_started.load(Ordering::SeqCst) {
            self.connected_started_notify.notified().await;
        }
    }

    fn release_connected_event(&self) {
        self.release_connected.notify_waiters();
    }

    fn events(&self) -> std::io::Result<Vec<WebrtcConnectionState>> {
        self.events
            .lock()
            .map(|events| events.iter().map(|(_, state)| *state).collect())
            .map_err(|_| std::io::Error::other("events poisoned"))
    }

    fn peer_events(&self) -> std::io::Result<Vec<(Did, WebrtcConnectionState)>> {
        self.events
            .lock()
            .map(|events| events.clone())
            .map_err(|_| std::io::Error::other("events poisoned"))
    }

    fn blocks_connected_peer(&self, peer: Did) -> bool {
        self.blocked_peer
            .lock()
            .map(|blocked| match *blocked {
                Some(blocked) => blocked == peer,
                None => true,
            })
            .unwrap_or(true)
    }
}

#[cfg(feature = "dummy")]
#[async_trait]
impl SwarmCallback for BlockingEventSwarmCallback {
    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), crate::error::CallbackError> {
        let SwarmEvent::ConnectionStateChange { peer, state } = event;
        match self.events.lock() {
            Ok(mut events) => events.push((*peer, *state)),
            Err(_) => tracing::error!("BlockingEventSwarmCallback events mutex is poisoned"),
        }
        if *state == WebrtcConnectionState::Connected && self.blocks_connected_peer(*peer) {
            self.connected_started.store(true, Ordering::SeqCst);
            self.connected_started_notify.notify_waiters();
            self.release_connected.notified().await;
        }
        Ok(())
    }
}

fn transport_with_key_and_measure(key: &SecretKey, measure: MeasureImpl) -> Result<SwarmTransport> {
    transport_with_key_measure_and_reassembly_limits(key, measure, ReassemblyLimits::production())
}

fn transport_with_key_measure_and_reassembly_limits(
    key: &SecretKey,
    measure: MeasureImpl,
    reassembly_limits: ReassemblyLimits,
) -> Result<SwarmTransport> {
    let session_sk = SessionSk::new_with_seckey(key)?;
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
        SwarmTransportSettings::new(1, VirtualNodeConfig::disabled(), reassembly_limits),
    ))
}

fn transport_with_measure(measure: MeasureImpl) -> Result<SwarmTransport> {
    transport_with_key_and_measure(&SecretKey::random(), measure)
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
fn transport_with_measure_and_reassembly_limits(
    measure: MeasureImpl,
    reassembly_limits: ReassemblyLimits,
) -> Result<SwarmTransport> {
    transport_with_key_measure_and_reassembly_limits(
        &SecretKey::random(),
        measure,
        reassembly_limits,
    )
}

#[cfg(feature = "dummy")]
async fn open_dummy_data_channel_before_ice_connected(
    transport: &SwarmTransport,
    peer: Did,
) -> Result<()> {
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
    Ok(())
}

#[test]
fn test_swarm_builder_uses_chord_virtual_node_default() -> Result<()> {
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
fn test_swarm_builder_normalizes_virtual_nodes_before_protocol_advertisement() -> Result<()> {
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

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_successor_failover_considers_active_peer_outside_topology_hints() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let removed = SecretKey::random().address().into();
    let replacement = SecretKey::random().address().into();

    for peer in [removed, replacement] {
        let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
        let (attempt, _offer) = transport
            .prepare_connection_offer_with_attempt(peer, callback)
            .await?;
        open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
        assert!(transport.activate_connection_for_test(attempt)?);
    }
    transport.dht.join(removed)?;
    assert_eq!(transport.dht.successors().list()?, vec![removed]);
    assert!(!transport.dht.lock_finger()?.contains(Some(replacement)));

    assert_eq!(
        transport.live_successor_fallback(removed)?,
        Some(replacement)
    );

    transport.disconnect(removed).await?;
    transport.disconnect(replacement).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_data_channel_open_admits_successor_before_ice_connected() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback))
        .with_pending_connection_attempt(attempt);
    callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection(peer));
    assert!(transport.get_connection(peer).is_some());
    assert!(
        transport.dht.successors().contains(&peer)?,
        "opened data channel must promote the advertised peer into DHT successors"
    );

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_pending_callback_messages_do_not_dispatch_before_admission() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
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
    let bytes = payload.to_wire()?;

    pending_callback
        .on_admitted_message_for_test(&peer.to_string(), &bytes)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(app_callback.validates(), 0);
    assert_eq!(app_callback.inbounds(), 0);
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&peer)?);
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

    pending_callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;
    pending_callback
        .on_admitted_message_for_test(&peer.to_string(), &bytes)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 1);
    let counters = measure.snapshot_counters()?;
    assert!(counters.contains(&(peer, MeasureCounter::Connect)));
    assert!(counters.contains(&(peer, MeasureCounter::Received)));
    assert!(transport.is_admitted_connection(peer));

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_nested_reassembled_chunk_is_rejected_without_recursive_callback_entry() -> Result<()>
{
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let mut current: Bytes = MessagePayload::new_send(
        Message::custom(b"mailbox-drained")?,
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?
    .to_wire()?;
    for _ in 0..2 {
        let chunk = Chunk {
            chunk: [0, 1],
            data: current,
            meta: ChunkMeta::default(),
        };
        current = MessagePayload::new_send(
            Message::Chunk(chunk),
            &peer_session,
            transport.dht.did,
            transport.dht.did,
        )?
        .to_wire()?;
    }

    reset_on_message_recursion_depth_for_test();
    let error = callback
        .on_admitted_message_for_test(&peer.to_string(), &current)
        .await
        .expect_err("nested chunks must be rejected");

    assert!(error.to_string().contains("Nested chunk"));
    assert_eq!(app_callback.validates(), 1);
    assert_eq!(app_callback.inbounds(), 0);
    assert_eq!(max_on_message_recursion_depth_for_test(), 1);
    assert_eq!(
        measure
            .snapshot_counters()?
            .into_iter()
            .filter(|(did, counter)| {
                *did == peer && *counter == MeasureCounter::FailedToReceive
            })
            .count(),
        1
    );
    assert_eq!(callback.inbound_admitted_count_for_test(), 0);
    assert_eq!(transport.reassembly_budget().buffered_cost_for_test(), 0);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[tokio::test]
async fn test_missing_peer_error_precedes_outbound_capacity_admission() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = Did::from(700_u32);
    let mut permits = Vec::with_capacity(OUTBOUND_DATA_TRANSFER_CAPACITY);
    for _ in 0..OUTBOUND_DATA_TRANSFER_CAPACITY {
        permits.push(
            transport
                .outbound_schedulers
                .reserve(peer, MessageClass::Application, 1)
                .await?,
        );
    }
    let payload = MessagePayload::new_send(
        Message::custom(b"missing-peer")?,
        transport.session_sk(),
        peer,
        peer,
    )?;

    let error = transport
        .send_payload_detached_with_outcome(payload)
        .await
        .expect_err("missing route must be checked before capacity");

    assert!(matches!(error, Error::SwarmMissDidInTable(did) if did == peer));
    drop(permits);
    Ok(())
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[traced_test]
#[tokio::test]
async fn test_invalid_inbound_log_omits_transaction_data() -> Result<()> {
    const PRIVATE_MARKER: &str = "PRIVATE-INBOUND-PAYLOAD-7f34c91a";
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer_key = SecretKey::random();
    let peer: Did = peer_key.address().into();
    let peer_session = SessionSk::new_with_seckey(&peer_key)?;
    let callback = InnerSwarmCallback::new(Arc::clone(&transport), Arc::new(NoopSwarmCallback));
    let mut payload = MessagePayload::new_send(
        Message::custom(PRIVATE_MARKER.as_bytes())?,
        &peer_session,
        transport.dht.did,
        transport.dht.did,
    )?;
    payload.transaction.data.push(171);
    let expected_tx_id = payload.transaction.tx_id;
    let expected_destination = payload.transaction.destination;
    let expected_data_bytes = payload.transaction.data.len();
    let bytes = payload.to_wire()?;
    let expected_wire_bytes = bytes.len();

    callback
        .on_admitted_message_for_test(&peer.to_string(), &bytes)
        .await
        .expect_err("mutating signed transaction data must fail verification");

    let expected_fields = [
        ("peer", format!("Some({peer:?})")),
        ("destination", expected_destination.to_string()),
        ("message_kind", "\"CustomMessage\"".to_owned()),
        ("data_bytes", expected_data_bytes.to_string()),
        ("wire_bytes", expected_wire_bytes.to_string()),
    ];
    let expected_tx_id = expected_tx_id.to_string();
    let marker_debug = crate::tests::byte_debug_fragment(PRIVATE_MARKER.as_bytes());
    logs_assert(|lines: &[&str]| {
        crate::tests::assert_single_structured_log_event(
            lines,
            "rings_core::swarm::callback",
            "inbound message verification failed or expired",
            ("tx_id", &expected_tx_id),
            &expected_fields,
            &[
                PRIVATE_MARKER,
                &marker_debug,
                "transaction.data",
                "MessagePayload {",
            ],
        )
    });
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_pending_disconnected_before_data_channel_open_is_not_reported() -> Result<()> {
    let measure = Arc::new(RecordingMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    let pending_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);

    pending_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Disconnected)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert_eq!(transport.pending_connection_count()?, 1);
    assert_eq!(app_callback.events()?, Vec::new());
    assert_eq!(measure.snapshot_counters()?, Vec::new());
    assert!(!transport.dht.successors().contains(&peer)?);

    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    pending_callback
        .on_data_channel_open(&peer.to_string())
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(transport.is_admitted_connection_attempt(attempt));
    let events = app_callback.events()?;
    assert_eq!(events, vec![
        WebrtcConnectionState::Connecting,
        WebrtcConnectionState::Connected,
    ]);
    assert!(transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn test_terminal_event_during_pending_admission_prevents_late_dht_join() -> Result<()> {
    let measure = Arc::new(BlockingConnectMeasure::default());
    let transport = Arc::new(transport_with_measure(measure.clone())?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(CountingSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;
    app_callback.clear_events()?;

    let opening_transport = Arc::clone(&transport);
    let opening_callback = app_callback.clone();
    let opening = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(opening_transport, opening_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    measure.wait_for_connect_started().await;
    assert!(transport.is_admitted_connection_attempt(attempt));

    let terminal_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone())
        .with_pending_connection_attempt(attempt);
    terminal_callback
        .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    measure.release_connect();
    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(!transport.is_admitted_connection_attempt(attempt));
    assert!(!transport.dht.successors().contains(&peer)?);
    let events = app_callback.events()?;
    assert!(events.contains(&WebrtcConnectionState::Closed));
    assert!(!events.contains(&WebrtcConnectionState::Connected));
    Ok(())
}
