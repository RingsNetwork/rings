use std::collections::BTreeMap;
#[cfg(feature = "dummy")]
use std::sync::atomic::AtomicBool;
#[cfg(feature = "dummy")]
use std::sync::atomic::AtomicUsize;
#[cfg(feature = "dummy")]
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use rings_transport::core::callback::TransportCallback;
#[cfg(feature = "dummy")]
use tokio::sync::Notify;

use super::pending::ConnectionLifecycleRegistry;
#[cfg(feature = "dummy")]
use super::pending::FingerUpdateDisposition;
use super::*;
use crate::dht::successor::SuccessorReader;
#[cfg(feature = "dummy")]
use crate::dht::Chord;
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
#[cfg(feature = "dummy")]
use crate::swarm::callback::SwarmEvent;
use crate::swarm::SwarmBuilder;

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod finger;
mod lifecycle;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
mod readiness;
mod retirement;

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

    async fn on_event(
        &self,
        event: &SwarmEvent,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
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
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
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
        SwarmTransportSettings::new(
            1,
            VirtualNodeConfig::disabled(),
            ReassemblyLimits::production(),
        ),
    ))
}

fn transport_with_measure(measure: MeasureImpl) -> Result<SwarmTransport> {
    transport_with_key_and_measure(&SecretKey::random(), measure)
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

#[cfg(feature = "dummy")]
#[tokio::test]
async fn successor_failover_considers_active_peer_outside_topology_hints() -> Result<()> {
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
async fn data_channel_open_admits_successor_before_ice_connected() -> Result<()> {
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
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

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
    let counters = measure.snapshot_counters()?;
    assert!(counters.contains(&(peer, MeasureCounter::Connect)));
    assert!(counters.contains(&(peer, MeasureCounter::Received)));
    assert!(transport.is_admitted_connection(peer));

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn pending_disconnected_before_data_channel_open_is_not_reported() -> Result<()> {
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
    assert_eq!(app_callback.events()?, vec![
        WebrtcConnectionState::Connected
    ]);
    assert!(transport.dht.successors().contains(&peer)?);

    transport.disconnect(peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn terminal_event_during_pending_admission_prevents_late_dht_join() -> Result<()> {
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

#[cfg(feature = "dummy")]
#[tokio::test]
async fn terminal_event_starts_in_order_without_waiting_for_connected_callback() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let peer = SecretKey::random().address().into();
    let app_callback = Arc::new(BlockingEventSwarmCallback::default());
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, peer).await?;

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

    app_callback.wait_for_connected_event_started().await;
    let terminal_transport = Arc::clone(&transport);
    let terminal_callback = app_callback.clone();
    let terminal = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(terminal_transport, terminal_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_peer_connection_state_change(&peer.to_string(), WebrtcConnectionState::Closed)
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), terminal)
        .await
        .map_err(|_| {
            Error::InvalidMessage("terminal event was blocked by application callback".to_string())
        })?
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;
    assert_eq!(connected_and_closed_events(app_callback.events()?), vec![
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Closed
    ]);
    app_callback.release_connected_event();

    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;
    assert_eq!(connected_and_closed_events(app_callback.events()?), vec![
        WebrtcConnectionState::Connected,
        WebrtcConnectionState::Closed
    ]);
    assert!(!transport.is_admitted_connection_attempt(attempt));
    assert!(!transport.dht.successors().contains(&peer)?);
    Ok(())
}

#[cfg(feature = "dummy")]
#[tokio::test]
async fn slow_connected_event_for_one_peer_does_not_block_other_peer_events() -> Result<()> {
    let transport = Arc::new(transport_with_measure(Arc::new(
        RecordingMeasure::default(),
    ))?);
    let blocked_peer: Did = SecretKey::random().address().into();
    let other_peer: Did = SecretKey::random().address().into();
    let app_callback = Arc::new(BlockingEventSwarmCallback::blocking_peer(blocked_peer));
    let offer_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    let (attempt, _offer) = transport
        .prepare_connection_offer_with_attempt(blocked_peer, offer_callback)
        .await?;
    open_dummy_data_channel_before_ice_connected(&transport, blocked_peer).await?;

    let opening_transport = Arc::clone(&transport);
    let opening_callback = app_callback.clone();
    let opening = tokio::spawn(async move {
        let callback = InnerSwarmCallback::new(opening_transport, opening_callback)
            .with_pending_connection_attempt(attempt);
        callback
            .on_data_channel_open(&blocked_peer.to_string())
            .await
            .map_err(|error| Error::InvalidMessage(error.to_string()))
    });

    app_callback.wait_for_connected_event_started().await;
    let other_callback = InnerSwarmCallback::new(Arc::clone(&transport), app_callback.clone());
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        other_callback.on_peer_connection_state_change(
            &other_peer.to_string(),
            WebrtcConnectionState::Connecting,
        ),
    )
    .await
    .map_err(|_| Error::InvalidMessage("unrelated peer event was blocked".to_string()))?
    .map_err(|error| Error::InvalidMessage(error.to_string()))?;

    assert!(app_callback
        .peer_events()?
        .contains(&(other_peer, WebrtcConnectionState::Connecting)));
    app_callback.release_connected_event();
    opening
        .await
        .map_err(|error| Error::InvalidMessage(error.to_string()))??;

    assert!(app_callback
        .peer_events()?
        .contains(&(blocked_peer, WebrtcConnectionState::Connected)));
    transport.disconnect(blocked_peer).await?;
    Ok(())
}

#[cfg(feature = "dummy")]
fn connected_and_closed_events(events: Vec<WebrtcConnectionState>) -> Vec<WebrtcConnectionState> {
    events
        .into_iter()
        .filter(|state| {
            matches!(
                state,
                WebrtcConnectionState::Connected | WebrtcConnectionState::Closed
            )
        })
        .collect()
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
    assert!(transport.activate_connection_for_test(current_attempt)?);
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
    let peer: Did = SecretKey::random().address().into();
    let attempt = transport.reserve_pending_connection(peer).await?;
    assert!(transport.activate_connection_for_test(attempt)?);

    transport.record_peer_disconnected(attempt).await;
    transport.record_peer_disconnected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_some());
    transport.record_peer_connected(attempt).await;
    assert!(transport.peer_disconnected_since_ms(peer).is_none());
    transport.record_peer_disconnected(attempt).await;
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

#[tokio::test]
async fn malformed_outbound_payload_is_rejected_before_connection_admission() -> Result<()> {
    let transport = transport_with_measure(Arc::new(RecordingMeasure::default()))?;
    let peer = SecretKey::random().address().into();
    let mut payload = MessagePayload::new_send(
        Message::custom(b"malformed outbound payload")?,
        &transport.session_sk,
        peer,
        peer,
    )?;
    payload.transaction.data = vec![0xff];

    assert!(matches!(
        transport.send_payload(payload).await,
        Err(Error::BincodeDeserialize(_))
    ));
    Ok(())
}
