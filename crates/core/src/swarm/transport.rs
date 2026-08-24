use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use rings_transport::connection_ref::ConnectionRef;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use rings_transport::connections::DummyConnection as ConnectionOwner;
#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
pub use rings_transport::connections::DummyTransport as Transport;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
pub use rings_transport::connections::WebSysWebrtcTransport as Transport;
#[cfg(all(
    not(all(feature = "wasm", target_family = "wasm")),
    not(feature = "dummy")
))]
use rings_transport::connections::WebrtcConnection as ConnectionOwner;
#[cfg(all(
    not(all(feature = "wasm", target_family = "wasm")),
    not(feature = "dummy")
))]
use rings_transport::connections::WebrtcTransport as Transport;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::SendPermit;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;
use rings_transport::webrtc_config::WebrtcUdpPortRange;

use self::storage_sync::StorageSyncAckMap;
use crate::chunk::ReassemblyBudget;
use crate::chunk::ReassemblyLimits;
use crate::dht::Did;
use crate::dht::LiveDid;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDeliveryCursor;
use crate::dht::VirtualNodeConfig;
use crate::error::Error;
use crate::error::Result;
use crate::measure::order_peers_by_quality;
use crate::measure::MeasureCounter;
use crate::measure::MeasureImpl;
use crate::measure::PeerMeasurement;
use crate::measure::PeerQuality;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::DhtProtocolMode;
use crate::message::Message;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;

mod connection;
mod delivery;
mod event_delivery;
mod liveness;
mod outbound;
mod payload_send;
mod pending;
mod readiness;
mod storage_lookup;
mod storage_sync;
mod timeouts;

pub(crate) use self::connection::AdmittedConnection;
use self::delivery::record_measurement;
use self::event_delivery::PeerOperationLocks;
use self::event_delivery::SwarmEventDeliveryLock;
use self::event_delivery::SwarmEventDeliveryLocks;
pub(crate) use self::event_delivery::SwarmEventDeliveryTurn;
use self::liveness::PeerLivenessMap;
pub(crate) use self::liveness::PEER_LIVENESS_IDLE_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::liveness::PEER_LIVENESS_TIMEOUT_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::outbound_submit_count_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::reset_outbound_submit_count_for_test;
use self::outbound::OutboundSchedulers;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_DATA_TRANSFER_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
pub(crate) use self::pending::ConnectionEventDisposition;
use self::pending::ConnectionLifecycleBoundary;
pub(crate) use self::pending::PendingConnectionAttempt;
use self::pending::PendingFingerUpdates;
use self::pending::RawConnectionOwner;
use self::pending::SharedConnectionLifecycles;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
use self::pending::PENDING_CONNECTION_TIMEOUT_MS;
pub(crate) use self::readiness::TransportReadiness;
use self::storage_lookup::StorageLookupObservationMap;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
pub(crate) use self::storage_lookup::STORAGE_LOOKUP_OBSERVATION_CAPACITY;
pub(crate) use self::storage_sync::TrackedStorageSyncOutcome;
pub(crate) use self::timeouts::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
use self::timeouts::TRANSPORT_TIMEOUT_PROFILE;
use super::callback::InboundCapacity;

pub struct SwarmTransport {
    pub(crate) network_id: u32,
    transport: Transport,
    session_sk: SessionSk,
    pub(crate) dht: Arc<PeerRing>,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
    reassembly_budget: Arc<ReassemblyBudget>,
    inbound_capacity: Arc<InboundCapacity>,
    connection_lifecycle: ConnectionLifecycleBoundary,
    swarm_event_delivery: SwarmEventDeliveryLocks,
    connection_creation: PeerOperationLocks,
    peer_lifecycles: SharedConnectionLifecycles,
    pending_finger_updates: Mutex<PendingFingerUpdates>,
    peer_liveness: Mutex<PeerLivenessMap>,
    storage_lookup_observations: Mutex<StorageLookupObservationMap>,
    pending_storage_sync_acks: Mutex<StorageSyncAckMap>,
    storage_repair_requested: AtomicBool,
    storage_repair_cursor: Mutex<Option<StorageSyncDeliveryCursor>>,
    outbound_schedulers: OutboundSchedulers,
    measured_disconnects: Mutex<BTreeMap<Did, (u64, i64)>>,
    measure: Option<MeasureImpl>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IncomingOfferAdmittedPeer {
    Vacant,
    Routable,
    Unroutable(PendingConnectionAttempt),
}

/// Runtime settings used by [`SwarmTransport`].
#[derive(Clone, Copy)]
pub(crate) struct SwarmTransportSettings {
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
}

impl SwarmTransportSettings {
    /// Build transport settings from DHT protocol parameters and chunk reassembly limits.
    pub(crate) fn new(
        storage_redundancy: u16,
        storage_virtual_node_config: VirtualNodeConfig,
        reassembly_limits: ReassemblyLimits,
    ) -> Self {
        Self {
            storage_redundancy,
            dht_virtual_nodes: storage_virtual_node_config.positions_per_owner(),
            reassembly_limits,
        }
    }
}

/// WebRTC transport configuration used when constructing [`SwarmTransport`].
pub(crate) struct SwarmWebrtcConfig {
    ice_servers: String,
    external_address: Option<String>,
    udp_port_range: Option<WebrtcUdpPortRange>,
}

impl SwarmWebrtcConfig {
    /// Build WebRTC transport configuration.
    ///
    /// Invariant: a present `udp_port_range` already proves `1 <= min <= max`.
    /// Native transports use it to constrain ICE UDP gathering; browser
    /// transports ignore it because local ICE ports are owned by the browser.
    pub(crate) fn new(
        ice_servers: String,
        external_address: Option<String>,
        udp_port_range: Option<WebrtcUdpPortRange>,
    ) -> Self {
        Self {
            ice_servers,
            external_address,
            udp_port_range,
        }
    }
}

/// Read-only handle to one physical transport connection.
///
/// Clone law: clones observe the same physical connection. A
/// `SwarmConnection` does not carry logical-generation authority; sending is
/// therefore private to the generation-bound [`AdmittedConnection`] workflow.
#[derive(Clone)]
pub struct SwarmConnection {
    peer: Did,
    connection: ConnectionRef<ConnectionOwner>,
}

impl SwarmTransport {
    pub(crate) fn new(
        network_id: u32,
        webrtc: SwarmWebrtcConfig,
        session_sk: SessionSk,
        dht: Arc<PeerRing>,
        measure: Option<MeasureImpl>,
        settings: SwarmTransportSettings,
    ) -> Self {
        Self {
            network_id,
            transport: Transport::new(
                &webrtc.ice_servers,
                webrtc.external_address,
                webrtc.udp_port_range,
            ),
            session_sk,
            dht,
            storage_redundancy: settings.storage_redundancy,
            dht_virtual_nodes: settings.dht_virtual_nodes,
            reassembly_limits: settings.reassembly_limits,
            reassembly_budget: Arc::new(ReassemblyBudget::new(settings.reassembly_limits)),
            inbound_capacity: Arc::new(InboundCapacity::new()),
            connection_lifecycle: ConnectionLifecycleBoundary::new(),
            swarm_event_delivery: SwarmEventDeliveryLocks::new(),
            connection_creation: PeerOperationLocks::new(),
            peer_lifecycles: Arc::new(
                Mutex::new(self::pending::ConnectionLifecycleRegistry::new()),
            ),
            pending_finger_updates: Mutex::new(BTreeMap::new()),
            peer_liveness: Mutex::new(PeerLivenessMap::new()),
            storage_lookup_observations: Mutex::new(BTreeMap::new()),
            pending_storage_sync_acks: Mutex::new(BTreeMap::new()),
            storage_repair_requested: AtomicBool::new(false),
            storage_repair_cursor: Mutex::new(None),
            outbound_schedulers: OutboundSchedulers::new(measure.clone()),
            measured_disconnects: Mutex::new(BTreeMap::new()),
            measure,
        }
    }

    /// Redundancy used by storage repair and anti-entropy.
    pub(crate) fn storage_redundancy(&self) -> u16 {
        self.storage_redundancy
    }

    /// Submit storage repair work to the shared maintenance loop.
    ///
    /// Invariant: a request submitted after the loop claims the previous
    /// request remains pending for a later repair run.
    pub(crate) fn request_storage_repair(&self) {
        self.storage_repair_requested.store(true, Ordering::Release);
    }

    /// Return whether storage repair work is waiting for maintenance capacity.
    pub(crate) fn storage_repair_requested(&self) -> bool {
        self.storage_repair_requested.load(Ordering::Acquire)
    }

    /// Atomically claim the currently pending storage repair work.
    pub(crate) fn claim_storage_repair(&self) -> bool {
        self.storage_repair_requested.swap(false, Ordering::AcqRel)
    }

    fn incoming_offer_admitted_peer(&self, peer: Did) -> Result<IncomingOfferAdmittedPeer> {
        self.with_connection_lifecycle(|| {
            let Some(attempt) = self.active_attempt(peer)? else {
                return Ok(IncomingOfferAdmittedPeer::Vacant);
            };
            if self.peer_lifecycles()?.sendable_attempt(peer) != Some(attempt) {
                return Ok(IncomingOfferAdmittedPeer::Unroutable(attempt));
            }
            let Some(connection) = self.get_raw_connection(peer) else {
                return Ok(IncomingOfferAdmittedPeer::Unroutable(attempt));
            };
            if connection.readiness().can_make_progress() {
                return Ok(IncomingOfferAdmittedPeer::Routable);
            }
            Ok(IncomingOfferAdmittedPeer::Unroutable(attempt))
        })
    }

    /// Select the next fair window over stable, ordered repair delivery keys.
    ///
    /// Invariant: the cursor is the last scheduled delivery key, not an ordinal
    /// in a transient list. Inserting or removing another delivery therefore
    /// cannot silently reset progress to the same surviving item. Repair plans
    /// are recomputed, so a deferred item remains eligible after other work.
    pub(crate) fn storage_repair_window_start(
        &self,
        ordered: &[StorageSyncDeliveryCursor],
        window: usize,
    ) -> Result<usize> {
        if ordered.is_empty() || window == 0 {
            return Ok(0);
        }
        let cursor = self
            .storage_repair_cursor
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let start = match cursor.as_ref() {
            Some(previous) => {
                let next = ordered.partition_point(|candidate| candidate <= previous);
                if next == ordered.len() {
                    0
                } else {
                    next
                }
            }
            None => 0,
        };
        Ok(start)
    }

    /// Advance the fair scheduling cursor after selecting one repair delivery.
    pub(crate) fn advance_storage_repair_cursor(
        &self,
        scheduled: StorageSyncDeliveryCursor,
    ) -> Result<()> {
        let mut cursor = self
            .storage_repair_cursor
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        *cursor = Some(scheduled);
        Ok(())
    }

    /// Storage virtual-node positions required by this DHT protocol mode.
    pub(crate) fn dht_virtual_nodes(&self) -> u16 {
        self.dht_virtual_nodes
    }

    fn dht_protocol_mode(&self) -> DhtProtocolMode {
        DhtProtocolMode::new(
            self.network_id,
            self.storage_redundancy,
            self.dht_virtual_nodes,
        )
    }

    /// Return whether an inbound connection offer matches this DHT protocol mode.
    pub(crate) fn accepts_connection_offer(&self, offer: &ConnectNodeSend) -> bool {
        offer.matches_dht_protocol(self.dht_protocol_mode())
    }

    /// Return whether an inbound connection answer matches this DHT protocol mode.
    pub(crate) fn accepts_connection_answer(&self, answer: &ConnectNodeReport) -> bool {
        answer.matches_dht_protocol(self.dht_protocol_mode())
    }

    /// Chunk reassembly limits enforced by inbound callbacks.
    pub(crate) fn reassembly_limits(&self) -> ReassemblyLimits {
        self.reassembly_limits
    }

    pub(crate) fn reassembly_budget(&self) -> Arc<ReassemblyBudget> {
        self.reassembly_budget.clone()
    }

    pub(crate) fn inbound_capacity(&self) -> Arc<InboundCapacity> {
        self.inbound_capacity.clone()
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn inbound_admitted_count_for_test(&self) -> usize {
        self.inbound_capacity.admitted_count_for_test()
    }

    async fn record_peer_measurement(&self, peer: Did, counter: MeasureCounter) {
        record_measurement(self.measure.clone(), peer, counter).await;
    }

    pub(crate) fn swarm_event_delivery_lock(&self, peer: Did) -> SwarmEventDeliveryLock {
        self.swarm_event_delivery.lock(peer)
    }

    pub(crate) fn prune_swarm_event_delivery_lock(
        &self,
        peer: Did,
        delivery: &SwarmEventDeliveryLock,
    ) {
        self.swarm_event_delivery
            .prune(peer, delivery, self.connection_epoch_exists(peer));
    }

    fn connection_epoch_exists(&self, peer: Did) -> bool {
        self.peer_lifecycles()
            .map(|lifecycles| lifecycles.contains(peer))
            .unwrap_or(false)
    }

    /// Record that `peer` reached an open data channel.
    pub(crate) async fn record_peer_connected(&self, attempt: PendingConnectionAttempt) {
        if !self.is_admitted_connection_attempt(attempt) {
            return;
        }
        self.mark_peer_liveness_connected(attempt);
        let updated = self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(false);
            }
            self.measured_disconnects
                .lock()
                .map_err(|_| Error::SwarmConnectionLifecycleLock)?
                .remove(&attempt.peer);
            Ok(true)
        });
        match updated {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(
                    "Failed to update disconnect epoch for connected peer {} generation {}: {error}",
                    attempt.peer,
                    attempt.generation
                );
                return;
            }
        }
        self.record_peer_measurement(attempt.peer, MeasureCounter::Connect)
            .await;
    }

    /// Record that `peer` left the usable connection epoch.
    ///
    /// Invariant: for one connection epoch, at most one `Disconnected` counter is
    /// recorded. `record_peer_connected` starts a new epoch by clearing the marker.
    pub(crate) async fn record_peer_disconnected(&self, attempt: PendingConnectionAttempt) {
        let now_ms = get_epoch_ms_i64();
        let should_record = match self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(false);
            }
            let mut measured = self
                .measured_disconnects
                .lock()
                .map_err(|_| Error::SwarmConnectionLifecycleLock)?;
            let previous = measured.insert(attempt.peer, (attempt.generation, now_ms));
            Ok(!matches!(
                previous,
                Some((generation, _)) if generation == attempt.generation
            ))
        }) {
            Ok(should_record) => should_record,
            Err(error) => {
                tracing::warn!(
                    "Failed to update disconnect epoch for peer {} generation {}: {error}",
                    attempt.peer,
                    attempt.generation
                );
                false
            }
        };
        if should_record {
            self.record_peer_measurement(attempt.peer, MeasureCounter::Disconnected)
                .await;
        }
    }

    /// Return the first time this peer left the usable connection epoch.
    pub(crate) fn peer_disconnected_since_attempt_ms(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Option<i64> {
        self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(None);
            }
            Ok(self
                .measured_disconnects
                .lock()
                .map_err(|_| Error::SwarmConnectionLifecycleLock)?
                .get(&attempt.peer)
                .filter(|(generation, _)| *generation == attempt.generation)
                .map(|(_, since_ms)| *since_ms))
        })
        .ok()
        .flatten()
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn peer_disconnected_since_ms(&self, peer: Did) -> Option<i64> {
        self.active_attempt(peer)
            .ok()
            .flatten()
            .and_then(|attempt| self.peer_disconnected_since_attempt_ms(attempt))
    }

    pub(crate) fn clear_peer_disconnected(&self, attempt: PendingConnectionAttempt) {
        if let Err(error) = self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(());
            }
            self.measured_disconnects
                .lock()
                .map_err(|_| Error::SwarmConnectionLifecycleLock)?
                .remove(&attempt.peer);
            Ok(())
        }) {
            tracing::warn!(
                "Failed to clear disconnect epoch for peer {} generation {}: {error}",
                attempt.peer,
                attempt.generation
            );
        }
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn force_peer_disconnected_since_ms(
        &self,
        peer: Did,
        disconnected_since_ms: i64,
    ) -> Result<()> {
        let Some(attempt) = self.active_attempt(peer)? else {
            return Ok(());
        };
        self.with_connection_lifecycle(|| {
            if !self.owns_active_slot(attempt)? {
                return Ok(());
            }
            self.measured_disconnects
                .lock()
                .map_err(|_| Error::SwarmConnectionLifecycleLock)?
                .insert(peer, (attempt.generation, disconnected_since_ms));
            Ok(())
        })
    }

    /// Record that a payload from `peer` was accepted and verified by the swarm.
    pub(crate) async fn record_peer_message_received(&self, attempt: PendingConnectionAttempt) {
        self.mark_peer_liveness_inbound(attempt);
        self.record_peer_measurement(attempt.peer, MeasureCounter::Received)
            .await;
    }

    /// Record that a payload from `peer` could not be decoded or verified.
    pub(crate) async fn record_peer_message_receive_failed(&self, peer: Did) {
        self.record_peer_measurement(peer, MeasureCounter::FailedToReceive)
            .await;
    }

    /// Record that an outbound payload to `peer` failed before delivery.
    pub(crate) async fn record_peer_message_send_failed(&self, peer: Did) {
        self.record_peer_measurement(peer, MeasureCounter::FailedToSend)
            .await;
    }

    /// Return this node's local quality judgement for `peer`.
    pub(crate) async fn peer_quality(&self, peer: Did) -> PeerQuality {
        match &self.measure {
            Some(measure) => measure.quality(peer).await,
            None => PeerQuality::Unknown,
        }
    }

    /// Return this node's local measurement counters for `peer`, if observed.
    pub(crate) async fn peer_measurement(&self, peer: Did) -> Option<PeerMeasurement> {
        match &self.measure {
            Some(measure) => PeerMeasurement::from_measure(measure.as_ref(), peer).await,
            None => None,
        }
    }

    /// Order DHT-produced connection candidates by local quality evidence.
    ///
    /// Invariant: this is a stable permutation of the DHT-produced candidate
    /// sequence. It changes attempt order only; it never changes Chord ownership,
    /// successor responsibility, or storage placement.
    pub(crate) async fn order_dht_candidates_by_quality(
        &self,
        candidates: impl IntoIterator<Item = Did>,
    ) -> Vec<Did> {
        let mut measured = Vec::new();
        for did in candidates {
            measured.push((did, self.peer_quality(did).await));
        }
        order_peers_by_quality(measured)
    }

    /// Ensure the storage API redundancy matches repair redundancy.
    pub(crate) fn ensure_storage_redundancy<const REDUNDANT: u16>(&self) -> Result<()> {
        self.ensure_storage_redundancy_value(REDUNDANT)
    }

    /// Validate that a runtime storage message uses this transport's redundancy.
    pub(crate) fn ensure_storage_redundancy_value(&self, redundancy: u16) -> Result<()> {
        if self.storage_redundancy == redundancy {
            Ok(())
        } else {
            Err(Error::StorageRedundancyMismatch {
                configured: self.storage_redundancy,
                requested: redundancy,
            })
        }
    }

    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        let (attempt, offer_msg) = match self
            .prepare_connection_offer_with_attempt(peer, callback)
            .await
        {
            Ok(offer) => offer,
            Err(Error::AlreadyConnected) => return Err(Error::AlreadyConnected),
            Err(e) => {
                if self.get_connection(peer).is_some() {
                    tracing::info!(
                        target: "rings_core::swarm::transport::handshake",
                        local = %self.dht.did,
                        peer = %peer,
                        error = ?e,
                        "connection request satisfied by concurrent handshake"
                    );
                    return Ok(());
                }
                self.record_peer_message_send_failed(peer).await;
                return Err(e);
            }
        };
        let sdp_len = offer_msg.sdp.len();
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = sdp_len,
            "connection offer send start"
        );
        match self
            .send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await
        {
            Ok(tx_id) => {
                tracing::info!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    tx_id = %tx_id,
                    "connection offer send complete"
                );
            }
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection offer send failed"
                );
                self.abandon_pending_connection(attempt, "sending connection offer")
                    .await;
                if self.get_connection(peer).is_some() {
                    tracing::info!(
                        target: "rings_core::swarm::transport::handshake",
                        local = %self.dht.did,
                        peer = %peer,
                        generation = attempt.generation,
                        error = ?error,
                        "connection offer send failure satisfied by concurrent handshake"
                    );
                    return Ok(());
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Create new connection and its offer.
    pub async fn prepare_connection_offer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<ConnectNodeSend> {
        self.prepare_connection_offer_with_attempt(peer, callback)
            .await
            .map(|(_, offer)| offer)
    }

    async fn prepare_connection_offer_with_attempt(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<(PendingConnectionAttempt, ConnectNodeSend)> {
        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection offer create start"
        );
        let offer = match conn.connection.webrtc_create_offer().await {
            Ok(offer) => offer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection offer create failed"
                );
                self.abandon_pending_connection(attempt, "creating connection offer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection offer create complete"
        );
        let offer_str = match serde_json::to_string(&offer) {
            Ok(offer) => offer,
            Err(_) => {
                self.abandon_pending_connection(attempt, "serializing connection offer")
                    .await;
                return Err(Error::SerializeToString);
            }
        };
        let offer_msg = ConnectNodeSend {
            sdp: offer_str,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
        };

        Ok((attempt, offer_msg))
    }

    async fn reconcile_incoming_offer_peer(&self, peer: Did) -> Result<()> {
        self.expire_pending_connections().await?;
        match self.incoming_offer_admitted_peer(peer)? {
            IncomingOfferAdmittedPeer::Vacant => {}
            IncomingOfferAdmittedPeer::Routable => return Err(Error::AlreadyConnected),
            IncomingOfferAdmittedPeer::Unroutable(attempt) => {
                if self.disconnect_unavailable(attempt).await?.is_none()
                    && self.is_admitted_connection(peer)
                {
                    return Err(Error::AlreadyConnected);
                }
            }
        }

        if let Some(swarm_conn) = self.get_raw_connection(peer) {
            // Simultaneous offers use DID order: the larger local DID abandons
            // its pending offer. A raw connection without a lifecycle owner is
            // stale physical state and is removed only by exact identity.
            match self.raw_connection_owner(peer)? {
                RawConnectionOwner::Pending(attempt)
                    if swarm_conn.connection.webrtc_connection_state()
                        == WebrtcConnectionState::New
                        && self.dht.did > peer =>
                {
                    if !self.cancel_pending_connection(attempt).await? {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Orphan => {
                    if !self
                        .transport
                        .close_connection_if_current(&swarm_conn.connection)
                        .await
                        .map_err(Error::Transport)?
                    {
                        return Err(Error::AlreadyConnected);
                    }
                }
                RawConnectionOwner::Pending(_) | RawConnectionOwner::Owned => {
                    return Err(Error::AlreadyConnected);
                }
            }
        }
        Ok(())
    }

    async fn create_connection_answer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer: String,
    ) -> Result<ConnectNodeReport> {
        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        let pending_connection = self.new_pending_connection(attempt, callback).await?;
        let attempt = pending_connection.attempt();
        let conn = pending_connection.connection();

        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            offer_sdp_bytes = offer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create start"
        );
        let answer = match conn.connection.webrtc_answer_offer(offer).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!(
                    target: "rings_core::swarm::transport::handshake",
                    local = %self.dht.did,
                    peer = %peer,
                    generation = attempt.generation,
                    error = ?error,
                    "connection answer create failed"
                );
                self.abandon_pending_connection(attempt, "creating connection answer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer create complete"
        );
        let answer_str = match serde_json::to_string(&answer) {
            Ok(answer) => answer,
            Err(_) => {
                self.abandon_pending_connection(attempt, "serializing connection answer")
                    .await;
                return Err(Error::SerializeToString);
            }
        };
        let answer_msg = ConnectNodeReport {
            sdp: answer_str,
            network_id: self.network_id,
            storage_redundancy: self.storage_redundancy,
            dht_virtual_nodes: self.dht_virtual_nodes,
        };

        Ok(answer_msg)
    }

    /// Answer the offer of remote connection.
    pub async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        if !self.accepts_connection_offer(offer_msg) {
            return Err(Error::InvalidMessage(
                "connection offer DHT protocol mismatch".to_string(),
            ));
        }
        let offer: String = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;
        self.reconcile_incoming_offer_peer(peer).await?;
        self.create_connection_answer(peer, callback, offer).await
    }

    /// Accept the answer of remote connection.
    pub async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
    ) -> Result<()> {
        if !self.accepts_connection_answer(answer_msg) {
            return Err(Error::InvalidMessage(
                "connection answer DHT protocol mismatch".to_string(),
            ));
        }

        let answer: String = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let (attempt, conn) = self
            .pending_connection_with_attempt(peer)?
            .ok_or(Error::SwarmMissTransport(peer))?;
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer accept start"
        );
        if let Err(error) = conn.connection.webrtc_accept_answer(answer).await {
            self.abandon_pending_connection(attempt, "accepting connection answer")
                .await;
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %peer,
                error = ?error,
                "connection answer accept failed"
            );
            return Err(Error::Transport(error));
        }
        if !self.is_current_connection_attempt(attempt)? {
            return Err(Error::ConnectionAttemptSuperseded {
                peer,
                generation: attempt.generation,
            });
        }
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = attempt.generation,
            state = ?conn.webrtc_connection_state(),
            "connection answer accept complete"
        );

        Ok(())
    }
}

impl SwarmConnection {
    async fn send_data(&self, data: Bytes, permit: SendPermit) -> Result<DeliveryFuture> {
        self.connection
            .send_message_with_permit(TransportMessage::Custom(data), permit)
            .await
            .map_err(|e| e.into())
    }

    async fn close(&self) -> Result<()> {
        self.connection.close().await.map_err(Into::into)
    }

    pub fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.connection.webrtc_connection_state()
    }

    /// The largest single data-channel message this connection can carry — the negotiated
    /// `max_message_size`. Used to size payload chunks so each wrapped chunk stays within the limit.
    pub fn max_message_size(&self) -> usize {
        self.connection.max_message_size()
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl LiveDid for SwarmConnection {
    async fn live(&self) -> bool {
        self.readiness().can_make_progress()
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}

#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
mod tests;
