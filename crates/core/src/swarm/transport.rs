use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
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
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;
use rings_transport::webrtc_config::WebrtcUdpPortRange;

use self::storage_sync::StorageSyncAckMap;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::chunk::ReassemblyLimits;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::Did;
use crate::dht::LiveDid;
use crate::dht::PeerRing;
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
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::swarm::callback::InnerSwarmCallback;
use crate::utils::get_epoch_ms_i64;
use crate::utils::sleep;

mod delivery;
mod liveness;
mod pending;
mod storage_lookup;
mod storage_sync;

use self::delivery::frame_chunk;
use self::delivery::record_measurement;
use self::delivery::send_data_with_timeout;
use self::delivery::spawn_chunked_send;
use self::delivery::spawn_delivery;
use self::delivery::ChunkSendPermit;
use self::liveness::PeerLivenessMap;
pub(crate) use self::liveness::PEER_LIVENESS_IDLE_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::liveness::PEER_LIVENESS_TIMEOUT_MS;
pub(crate) use self::pending::PendingConnectionAttempt;
use self::pending::PendingPeerPool;
pub(crate) use self::pending::DEFAULT_PENDING_CONNECTION_CAPACITY;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
use self::pending::PENDING_CONNECTION_TIMEOUT_MS;
use self::storage_lookup::StorageLookupObservationMap;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
pub(crate) use self::storage_lookup::STORAGE_LOOKUP_OBSERVATION_CAPACITY;

const DATA_CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(8);

fn message_kind(message: &Message) -> &'static str {
    match message {
        Message::ConnectNodeSend(_) => "ConnectNodeSend",
        Message::ConnectNodeReport(_) => "ConnectNodeReport",
        Message::FindSuccessorSend(_) => "FindSuccessorSend",
        Message::FindSuccessorReport(_) => "FindSuccessorReport",
        Message::NotifyPredecessorSend(_) => "NotifyPredecessorSend",
        Message::NotifyPredecessorReport(_) => "NotifyPredecessorReport",
        Message::PeerLivenessProbe(_) => "PeerLivenessProbe",
        Message::PeerLivenessReport(_) => "PeerLivenessReport",
        Message::SearchEntry(_) => "SearchEntry",
        Message::FoundEntry(_) => "FoundEntry",
        Message::OperateEntry(_) => "OperateEntry",
        Message::SyncEntriesWithSuccessor(_) => "SyncEntriesWithSuccessor",
        Message::SyncEntriesWithSuccessorReport(_) => "SyncEntriesWithSuccessorReport",
        Message::CustomMessage(_) => "CustomMessage",
        Message::E2eHandshakeRequest(_) => "E2eHandshakeRequest",
        Message::E2eHandshakeResponse(_) => "E2eHandshakeResponse",
        Message::E2eStreamFrame(_) => "E2eStreamFrame",
        Message::QueryForTopoInfoSend(_) => "QueryForTopoInfoSend",
        Message::QueryForTopoInfoReport(_) => "QueryForTopoInfoReport",
        Message::Chunk(_) => "Chunk",
    }
}

fn payload_message_kind(payload: &MessagePayload) -> &'static str {
    match payload.transaction.data::<Message>() {
        Ok(message) => message_kind(&message),
        Err(_) => "Unknown",
    }
}

pub struct SwarmTransport {
    pub(crate) network_id: u32,
    transport: Transport,
    session_sk: SessionSk,
    pub(crate) dht: Arc<PeerRing>,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
    pending_peers: Mutex<PendingPeerPool<DEFAULT_PENDING_CONNECTION_CAPACITY>>,
    active_peers: Mutex<BTreeMap<Did, u64>>,
    peer_liveness: Mutex<PeerLivenessMap>,
    storage_lookup_observations: Mutex<StorageLookupObservationMap>,
    pending_storage_sync_acks: Mutex<StorageSyncAckMap>,
    measured_disconnects: Mutex<BTreeMap<Did, i64>>,
    measure: Option<MeasureImpl>,
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

#[derive(Clone)]
pub struct SwarmConnection {
    peer: Did,
    pub connection: ConnectionRef<ConnectionOwner>,
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
            pending_peers: Mutex::new(PendingPeerPool::new()),
            active_peers: Mutex::new(BTreeMap::new()),
            peer_liveness: Mutex::new(PeerLivenessMap::new()),
            storage_lookup_observations: Mutex::new(BTreeMap::new()),
            pending_storage_sync_acks: Mutex::new(BTreeMap::new()),
            measured_disconnects: Mutex::new(BTreeMap::new()),
            measure,
        }
    }

    /// Redundancy used by storage repair and anti-entropy.
    pub(crate) fn storage_redundancy(&self) -> u16 {
        self.storage_redundancy
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

    async fn record_peer_measurement(&self, peer: Did, counter: MeasureCounter) {
        record_measurement(self.measure.clone(), peer, counter).await;
    }

    /// Record that `peer` reached an open data channel.
    pub(crate) async fn record_peer_connected(&self, peer: Did) {
        self.mark_peer_liveness_connected(peer);
        match self.measured_disconnects.lock() {
            Ok(mut measured) => {
                measured.remove(&peer);
            }
            Err(_) => {
                tracing::warn!("Failed to update disconnect epoch for connected peer {peer}");
            }
        }
        self.record_peer_measurement(peer, MeasureCounter::Connect)
            .await;
    }

    /// Record that `peer` left the usable connection epoch.
    ///
    /// Invariant: for one connection epoch, at most one `Disconnected` counter is
    /// recorded. `record_peer_connected` starts a new epoch by clearing the marker.
    pub(crate) async fn record_peer_disconnected(&self, peer: Did) {
        let now_ms = get_epoch_ms_i64();
        let should_record = match self.measured_disconnects.lock() {
            Ok(mut measured) => measured.insert(peer, now_ms).is_none(),
            Err(_) => {
                tracing::warn!("Failed to update disconnect epoch for disconnected peer {peer}");
                true
            }
        };
        if should_record {
            self.record_peer_measurement(peer, MeasureCounter::Disconnected)
                .await;
        }
    }

    /// Return the first time this peer left the usable connection epoch.
    pub(crate) fn peer_disconnected_since_ms(&self, peer: Did) -> Option<i64> {
        self.measured_disconnects
            .lock()
            .ok()
            .and_then(|measured| measured.get(&peer).copied())
    }

    /// Record that a payload from `peer` was accepted and verified by the swarm.
    pub(crate) async fn record_peer_message_received(&self, peer: Did) {
        self.mark_peer_liveness_inbound(peer);
        self.record_peer_measurement(peer, MeasureCounter::Received)
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

    /// Get an active, routable connection by DID.
    ///
    /// Pending and terminal physical transports are intentionally invisible here.
    /// Get an active, routable connection by DID.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.is_active_connection(peer)
            .then(|| self.get_raw_connection(peer))
            .flatten()
    }

    /// Get all active, routable transport connections.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.active_peer_ids()
            .into_iter()
            .filter_map(|peer| {
                self.get_connection(peer)
                    .map(|connection| (peer, connection))
            })
            .collect()
    }

    fn active_peer_ids(&self) -> Vec<Did> {
        self.active_peers()
            .map(|active| active.keys().copied().collect())
            .unwrap_or_default()
    }

    /// Return admitted transports, including a terminal connection that still
    /// needs lifecycle cleanup. This is deliberately internal: callers outside
    /// the swarm only observe routable connections through [`Self::get_connections`].
    pub(crate) fn admitted_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.active_peer_ids()
            .into_iter()
            .filter_map(|peer| {
                self.get_raw_connection(peer)
                    .map(|connection| (peer, connection))
            })
            .collect()
    }

    /// Return admitted DIDs, even if their raw transport object has already gone away.
    pub(crate) fn admitted_connection_ids(&self) -> Vec<Did> {
        self.active_peer_ids()
    }

    /// Get DIDs of active, routable connections.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.get_connections()
            .into_iter()
            .map(|(peer, _)| peer)
            .collect()
    }

    /// Disconnect a connection.
    ///
    /// Pending connections are never represented in the DHT, so cancelling one
    /// only closes its transport object. Active connections leave the DHT before
    /// the underlying WebRTC object is released.
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        if let Some(attempt) = self.pending_attempt(peer)? {
            self.cancel_pending_connection(attempt).await?;
            return Ok(());
        }

        let was_active = self.retire_active_connection(peer)?;
        if !was_active {
            self.transport
                .close_connection(&peer.to_string())
                .await
                .map_err(Error::Transport)?;
            return Ok(());
        }

        tracing::info!("removing {peer} from DHT");
        self.dht.remove(peer)?;
        self.close_connection_for_disconnect(peer).await
    }

    async fn close_connection_for_disconnect(&self, peer: Did) -> Result<()> {
        match self.transport.close_connection(&peer.to_string()).await {
            Ok(()) => Ok(()),
            Err(rings_transport::error::Error::ConnectionNotFound(_)) => {
                tracing::warn!(
                    peer = %peer,
                    "connection was already absent while disconnecting admitted peer"
                );
                Ok(())
            }
            Err(error) => Err(error.into()),
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

    /// Get an active connection by DID and verify that its data channel remains open.
    /// This method will return None if the connection is not active.
    /// It can wait for a transiently disconnected active connection to recover,
    /// but pending handshakes never reach this path.
    /// See more information about [rings_transport::core::transport::WebrtcConnectionState].
    /// See also method webrtc_wait_for_data_channel_open [rings_transport::core::transport::ConnectionInterface].
    pub async fn get_and_check_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.get_and_check_connection_with_timeout(peer, DATA_CHANNEL_OPEN_TIMEOUT)
            .await
    }

    pub(crate) async fn get_and_check_connection_with_timeout(
        &self,
        peer: Did,
        wait_timeout: Duration,
    ) -> Option<SwarmConnection> {
        let conn = self.get_connection(peer)?;

        let initial_state = conn.webrtc_connection_state();
        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            state = ?initial_state,
            timeout_ms = wait_timeout.as_millis(),
            "waiting for active connection data channel"
        );

        let failure = {
            let wait_for_open = conn.connection.webrtc_wait_for_data_channel_open().fuse();
            let timeout = sleep(wait_timeout).fuse();
            pin_mut!(wait_for_open, timeout);

            select! {
                result = wait_for_open => result.err().map(|e| format!("transport_wait_failed: {e:?}")),
                _ = timeout => Some("data_channel_open_wait_timeout".to_string()),
            }
        };

        if let Some(reason) = failure {
            let final_state = conn.webrtc_connection_state();
            tracing::warn!(
                target: "rings_core::transport::data_channel",
                local = %self.dht.did,
                peer = %peer,
                initial_state = ?initial_state,
                final_state = ?final_state,
                timeout_ms = wait_timeout.as_millis(),
                reason = %reason,
                "[get_and_check_connection] connection data channel not open, will be dropped"
            );

            if let Err(e) = self.disconnect(peer).await {
                tracing::error!(
                    target: "rings_core::transport::data_channel",
                    local = %self.dht.did,
                    peer = %peer,
                    reason = %reason,
                    "failed to close connection after data-channel wait failure: {e:?}"
                );
            }

            return None;
        };

        tracing::debug!(
            target: "rings_core::transport::data_channel",
            local = %self.dht.did,
            peer = %peer,
            state = ?conn.webrtc_connection_state(),
            "active connection data channel is open"
        );

        Some(conn)
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
        self.new_pending_connection(attempt, callback).await?;
        let Some(conn) = self.get_raw_connection(peer) else {
            self.abandon_pending_connection(attempt, "looking up the offer transport")
                .await;
            return Err(Error::SwarmMissTransport(peer));
        };

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

        self.expire_pending_connections().await?;
        if self.is_active_connection(peer) {
            return Err(Error::AlreadyConnected);
        }

        if let Some(swarm_conn) = self.get_raw_connection(peer) {
            // Solve the scenario of creating offers simultaneously.
            //
            // When both sides create_offer at the same time and trigger answer_offer of the other side,
            // they will got existed New state connection when answer_offer, which will prevent
            // it to create new connection to answer the offer.
            //
            // The party with a larger Did (ranked lower on the ring) should abandon their own offer and instead answer_offer to the other party.
            // The party with a smaller Did should reject answering the other party and report an Error::AlreadyConnected error.
            if swarm_conn.connection.webrtc_connection_state() == WebrtcConnectionState::New {
                // drop local offer and continue answer remote offer
                if self.dht.did > peer {
                    // this connection will replaced by new connection created bellow
                    let pending = self.pending_attempt(peer)?;
                    if let Some(attempt) = pending {
                        self.cancel_pending_connection(attempt).await?;
                    } else {
                        self.transport
                            .close_connection(&peer.to_string())
                            .await
                            .map_err(Error::Transport)?;
                    }
                } else {
                    // ignore remote offer, and refuse to answer remote offer
                    return Err(Error::AlreadyConnected);
                }
            } else {
                return Err(Error::AlreadyConnected);
            }
        }

        let attempt = self.reserve_pending_connection(peer).await?;
        let callback = callback.with_pending_connection_attempt(attempt);
        self.new_pending_connection(attempt, callback).await?;
        let Some(conn) = self.get_raw_connection(peer) else {
            self.abandon_pending_connection(attempt, "looking up the answer transport")
                .await;
            return Err(Error::SwarmMissTransport(peer));
        };

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

        if !self.is_pending_connection(peer)? {
            return Err(Error::SwarmMissTransport(peer));
        }

        let conn = self
            .get_raw_connection(peer)
            .ok_or(Error::SwarmMissTransport(peer))?;
        let attempt = self.pending_attempt(peer)?;
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = ?attempt.map(|attempt| attempt.generation),
            answer_sdp_bytes = answer.len(),
            state = ?conn.webrtc_connection_state(),
            "connection answer accept start"
        );
        if let Err(error) = conn.connection.webrtc_accept_answer(answer).await {
            let attempt = self.pending_attempt(peer)?;
            if let Some(attempt) = attempt {
                self.abandon_pending_connection(attempt, "accepting connection answer")
                    .await;
            }
            tracing::warn!(
                target: "rings_core::swarm::transport::handshake",
                local = %self.dht.did,
                peer = %peer,
                error = ?error,
                "connection answer accept failed"
            );
            return Err(Error::Transport(error));
        }
        tracing::info!(
            target: "rings_core::swarm::transport::handshake",
            local = %self.dht.did,
            peer = %peer,
            generation = ?attempt.map(|attempt| attempt.generation),
            state = ?conn.webrtc_connection_state(),
            "connection answer accept complete"
        );

        Ok(())
    }
}

impl SwarmConnection {
    pub async fn send_data(&self, data: Bytes) -> Result<DeliveryFuture> {
        self.connection
            .send_message(TransportMessage::Custom(data.to_vec()))
            .await
            .map_err(|e| e.into())
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
impl PayloadSender for SwarmTransport {
    fn session_sk(&self) -> &SessionSk {
        &self.session_sk
    }

    fn dht(&self) -> Arc<PeerRing> {
        self.dht.clone()
    }

    fn is_connected(&self, did: Did) -> bool {
        let Some(conn) = self.get_connection(did) else {
            return false;
        };
        conn.webrtc_connection_state() == WebrtcConnectionState::Connected
    }

    async fn do_send_payload(&self, did: Did, payload: MessagePayload) -> Result<()> {
        let Some(conn) = self.get_and_check_connection(did).await else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::SwarmMissDidInTable(did));
        };

        let message_kind = payload_message_kind(&payload);
        let tx_id = payload.transaction.tx_id;
        let destination = payload.transaction.destination;
        let relay_destination = payload.relay.destination;
        let next_hop = payload.relay.next_hop;
        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!(
                local = %self.dht.did,
                next_hop = %next_hop,
                destination = %destination,
                relay_destination = %relay_destination,
                tx_id = %tx_id,
                message_kind,
                bytes = data.len(),
                max_bytes = TRANSPORT_MAX_SIZE,
                "message payload is too large"
            );
            return Err(Error::MessageTooLarge(data.len()));
        }

        // The chunk-vs-whole decision is the pure `WireReserves::plan`, against this connection's
        // negotiated `max_message_size`; this block is only the effectful shell carrying it out.
        // `None` means the peer's limit is too small to carry even one useful chunk. Send admission is
        // bounded because a WebRTC data-channel queue under backpressure can otherwise leave this
        // future pending until the caller's outer timeout fires without a transport-level reason.
        let Some(plan) = WireReserves::PRODUCTION.plan(data.len(), conn.max_message_size()) else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::PeerMaxMessageSizeTooSmall(conn.max_message_size()));
        };
        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            bytes = data.len(),
            max_message_size = conn.max_message_size(),
            framing = ?plan,
            "send payload start"
        );
        let chunk_send_permit = ChunkSendPermit::for_payload(self.dht.clone(), did, &payload);
        match plan {
            Framing::Whole => match send_data_with_timeout(&conn, data, did, "whole_message").await
            {
                Ok(delivery) => spawn_delivery(delivery, did, self.measure.clone()),
                Err(e) => {
                    if e.records_peer_send_failure() {
                        self.record_peer_message_send_failed(did).await;
                    }
                    return Err(e);
                }
            },
            Framing::Chunked { chunk_size } => {
                // Frame and accept the FIRST chunk on the caller's path, so an immediate send
                // failure (the buffer rejecting the bytes) surfaces here exactly as it does for a
                // whole message — `await send_message` callers learn the send was admitted. The
                // first chunk's flush and every remaining chunk are then driven by one bounded
                // background task (`run_chunked_send`), preserving fire-and-forget for the rest.
                let mut chunks = ChunkList::stream(data, chunk_size);
                if let Some(first) = chunks.next() {
                    let first = frame_chunk(&self.session_sk, did, first)?;
                    match send_data_with_timeout(&conn, first, did, "chunked_first").await {
                        Ok(first_delivery) => {
                            spawn_chunked_send(
                                conn,
                                Box::new(chunks),
                                first_delivery,
                                self.session_sk.clone(),
                                did,
                                chunk_send_permit,
                                self.measure.clone(),
                            );
                        }
                        Err(e) => {
                            if e.records_peer_send_failure() {
                                self.record_peer_message_send_failed(did).await;
                            }
                            return Err(e);
                        }
                    }
                }
            }
        }

        tracing::debug!(
            local = %self.dht.did,
            next_hop = %next_hop,
            destination = %destination,
            relay_destination = %relay_destination,
            tx_id = %tx_id,
            message_kind,
            "send payload accepted"
        );

        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl LiveDid for SwarmConnection {
    async fn live(&self) -> bool {
        match self.connection.data_channel_is_open() {
            Ok(open) => open,
            Err(error) => {
                tracing::debug!(
                    peer = %self.peer,
                    error = ?error,
                    "failed to inspect data-channel liveness"
                );
                false
            }
        }
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}

#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
mod tests;
