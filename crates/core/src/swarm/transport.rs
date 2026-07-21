use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::FutureExt;
use futures::pin_mut;
use futures::select;
use futures_timer::Delay;
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
use crate::chunk::Chunk;
use crate::chunk::ChunkList;
use crate::chunk::Framing;
use crate::chunk::ReassemblyLimits;
use crate::chunk::WireReserves;
use crate::consts::TRANSPORT_MAX_SIZE;
use crate::dht::entry::PlacementMiss;
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

mod storage_sync;

const STORAGE_LOOKUP_OBSERVATION_TTL_MS: i64 = 30_000;
/// Maximum number of read-repair miss observation buckets retained per transport.
pub(crate) const STORAGE_LOOKUP_OBSERVATION_CAPACITY: usize = 1024;

/// Maximum number of peers that may be handshaking before a data channel opens.
pub(crate) const DEFAULT_PENDING_CONNECTION_CAPACITY: usize = 32;

const PENDING_CONNECTION_TIMEOUT_MS: i64 = 180_000;
const DATA_CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(8);

/// Identifies one pending handshake for a peer.
///
/// A peer can have a replacement handshake after a timeout. Callbacks carry
/// this token so a late callback from the replaced connection cannot promote
/// the newer handshake into the active routing set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingConnectionAttempt {
    peer: Did,
    generation: u64,
}

impl PendingConnectionAttempt {
    pub(crate) fn peer(self) -> Did {
        self.peer
    }
}

#[derive(Debug)]
struct PendingPeer {
    generation: u64,
    admitted_at_ms: i64,
}

/// Bounded, non-routable handshakes owned by the swarm lifecycle.
///
/// The pool deliberately has no DHT reference: a peer is visible to Chord
/// only after its data channel opens and the matching attempt is promoted.
struct PendingPeerPool<const MAX_PENDING: usize> {
    next_generation: u64,
    peers: BTreeMap<Did, PendingPeer>,
}

impl<const MAX_PENDING: usize> PendingPeerPool<MAX_PENDING> {
    fn new() -> Self {
        Self {
            next_generation: 0,
            peers: BTreeMap::new(),
        }
    }

    fn reserve(&mut self, peer: Did, now_ms: i64) -> Result<PendingConnectionAttempt> {
        if self.peers.contains_key(&peer) {
            return Err(Error::AlreadyConnected);
        }
        if self.peers.len() >= MAX_PENDING {
            return Err(Error::PendingConnectionCapacityExceeded {
                capacity: MAX_PENDING,
            });
        }

        self.next_generation = self.next_generation.wrapping_add(1);
        let attempt = PendingConnectionAttempt {
            peer,
            generation: self.next_generation,
        };
        self.peers.insert(
            peer,
            PendingPeer {
                generation: attempt.generation,
                admitted_at_ms: now_ms,
            },
        );
        Ok(attempt)
    }

    fn contains(&self, peer: Did) -> bool {
        self.peers.contains_key(&peer)
    }

    fn remove(&mut self, attempt: PendingConnectionAttempt) -> bool {
        let Some(peer) = self.peers.get(&attempt.peer) else {
            return false;
        };
        if peer.generation != attempt.generation {
            return false;
        }
        self.peers.remove(&attempt.peer);
        true
    }

    fn expire(&mut self, now_ms: i64) -> Vec<PendingConnectionAttempt> {
        let expired = self
            .peers
            .iter()
            .filter_map(|(peer, pending)| {
                (now_ms.saturating_sub(pending.admitted_at_ms) >= PENDING_CONNECTION_TIMEOUT_MS)
                    .then_some(PendingConnectionAttempt {
                        peer: *peer,
                        generation: pending.generation,
                    })
            })
            .collect::<Vec<_>>();
        for attempt in &expired {
            self.peers.remove(&attempt.peer);
        }
        expired
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    fn len(&self) -> usize {
        self.peers.len()
    }
}

// Invariant: after every successful observation-buffer mutation,
// observations.len() <= STORAGE_LOOKUP_OBSERVATION_CAPACITY.
// Invariant: after evict_storage_lookup_observations(observations, now), every
// retained bucket satisfies
// now.saturating_sub(observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS. This
// is the freshness witness required before PlacementMiss.owner drives read-repair.
type StorageLookupObservationMap = BTreeMap<StorageLookupObservationKey, StorageLookupObservation>;

pub struct SwarmTransport {
    pub(crate) network_id: u32,
    transport: Transport,
    session_sk: SessionSk,
    pub(crate) dht: Arc<PeerRing>,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
    pending_peers: Mutex<PendingPeerPool<DEFAULT_PENDING_CONNECTION_CAPACITY>>,
    active_peers: Mutex<BTreeSet<Did>>,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct StorageLookupObservationKey {
    resource: Did,
    redundancy: u16,
}

struct StorageLookupObservation {
    observed_at_ms: i64,
    misses: BTreeSet<PlacementMiss>,
}

fn storage_lookup_observation_now_ms() -> i64 {
    get_epoch_ms_i64()
}

fn oldest_storage_lookup_observation_key(
    observations: &StorageLookupObservationMap,
) -> Option<StorageLookupObservationKey> {
    observations
        .iter()
        .min_by_key(|(key, observation)| (observation.observed_at_ms, **key))
        .map(|(key, _)| *key)
}

// Post: observations.len() <= STORAGE_LOOKUP_OBSERVATION_CAPACITY.
// Post: forall bucket in observations,
// now_ms.saturating_sub(bucket.observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS.
// Preservation: removing expired buckets and then oldest buckets cannot create
// a stale bucket or increase the number of buckets.
fn evict_storage_lookup_observations(observations: &mut StorageLookupObservationMap, now_ms: i64) {
    observations.retain(|_, observation| {
        now_ms.saturating_sub(observation.observed_at_ms) <= STORAGE_LOOKUP_OBSERVATION_TTL_MS
    });

    while observations.len() > STORAGE_LOOKUP_OBSERVATION_CAPACITY {
        let Some(stale_key) = oldest_storage_lookup_observation_key(observations) else {
            break;
        };
        observations.remove(&stale_key);
    }
}

fn reserve_storage_lookup_observation_slot(observations: &mut StorageLookupObservationMap) {
    while observations.len() >= STORAGE_LOOKUP_OBSERVATION_CAPACITY {
        let Some(stale_key) = oldest_storage_lookup_observation_key(observations) else {
            break;
        };
        observations.remove(&stale_key);
    }
}

#[derive(Clone)]
pub struct SwarmConnection {
    peer: Did,
    pub connection: ConnectionRef<ConnectionOwner>,
}

async fn record_measurement(measure: Option<MeasureImpl>, did: Did, counter: MeasureCounter) {
    if let Some(measure) = measure {
        measure.incr(did, counter).await;
    }
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, recording
/// the eventual peer-quality observation. This keeps delivery tracking confined
/// to the send site: the status never propagates up through the swarm/node
/// layers.
#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_delivery(fut: DeliveryFuture, did: Did, measure: Option<MeasureImpl>) {
    wasm_bindgen_futures::spawn_local(async move {
        match fut.await {
            Ok(()) => record_measurement(measure, did, MeasureCounter::Sent).await,
            Err(e) => {
                tracing::warn!("Message to {did} was not delivered: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            }
        }
    });
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, recording
/// the eventual peer-quality observation.
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_delivery(fut: DeliveryFuture, did: Did, measure: Option<MeasureImpl>) {
    tokio::spawn(async move {
        match fut.await {
            Ok(()) => record_measurement(measure, did, MeasureCounter::Sent).await,
            Err(e) => {
                tracing::warn!("Message to {did} was not delivered: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
            }
        }
    });
}

/// Frame one chunk into the bytes a data-channel send carries: wrap it in a `MessagePayload`
/// addressed to `did` and serialize it. Pure (the only failure is serialization).
fn frame_chunk(session_sk: &SessionSk, did: Did, chunk: Chunk) -> Result<Bytes> {
    MessagePayload::new_send(Message::Chunk(chunk), session_sk, did, did)?.to_bincode()
}

/// The *tail* of a chunked message — every chunk after the first — yielded lazily. Boxed so the
/// background task owns a concrete, nameable type (`Send` off the browser, where spawned tasks must
/// be `Send`; single-threaded on it).
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
type ChunkTail = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(all(feature = "wasm", target_family = "wasm"))]
type ChunkTail = Box<dyn Iterator<Item = Chunk>>;

/// Drive the *tail* of a chunked send: the first chunk has already been accepted by the caller
/// (`do_send_payload`), so wait for it to flush (backpressure), then frame, send, and await each
/// remaining chunk in turn. One chunk is in flight at a time and no per-chunk task is spawned. A
/// later frame/send failure aborts the rest; the receiver TTL-expires the partial message (chunks
/// carry the message ttl), so no abort marker is needed. Fire-and-forget — the caller already
/// learned whether the *first* chunk was accepted, matching the whole-message contract.
async fn run_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    measure: Option<MeasureImpl>,
) {
    if let Err(e) = first_delivery.await {
        tracing::warn!("Chunked send to {did} stopped before the first chunk flushed: {e}");
        record_measurement(measure, did, MeasureCounter::FailedToSend).await;
        return;
    }
    for chunk in tail {
        let bytes = match frame_chunk(&session_sk, did, chunk) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!("Chunked send to {did} aborted while framing a chunk: {e}");
                return;
            }
        };
        match conn.send_data(bytes).await {
            Ok(delivery) => {
                if let Err(e) = delivery.await {
                    tracing::warn!("Chunked send to {did} stopped before flush: {e}");
                    record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                    return;
                }
            }
            Err(e) => {
                tracing::warn!("Chunked send to {did} stopped: {e}");
                record_measurement(measure, did, MeasureCounter::FailedToSend).await;
                return;
            }
        }
    }
    record_measurement(measure, did, MeasureCounter::Sent).await;
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(all(feature = "wasm", target_family = "wasm"))]
fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    measure: Option<MeasureImpl>,
) {
    wasm_bindgen_futures::spawn_local(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
        measure,
    ));
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(not(all(feature = "wasm", target_family = "wasm")))]
fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
    measure: Option<MeasureImpl>,
) {
    tokio::spawn(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
        measure,
    ));
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
            active_peers: Mutex::new(BTreeSet::new()),
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

    fn storage_lookup_observation_key(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<StorageLookupObservationKey> {
        self.ensure_storage_redundancy_value(redundancy)?;
        Ok(StorageLookupObservationKey {
            resource,
            redundancy,
        })
    }

    /// Start a fresh lookup round for `resource`.
    ///
    /// This replaces any previous miss observations for the same resource and
    /// redundancy with an empty local-authorized bucket. Inbound FoundEntry
    /// messages may only add misses to an existing bucket, so remote peers cannot
    /// create a new redundancy mode.
    ///
    /// Post: if capacity permits one active lookup, a bucket exists for
    /// `(resource, redundancy)` and contains no misses.
    /// Preservation: eviction establishes the capacity and freshness invariants
    /// before replacing the lookup-round bucket.
    pub(crate) fn start_storage_lookup(&self, resource: Did, redundancy: u16) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        reserve_storage_lookup_observation_slot(&mut observations);
        observations.insert(
            key,
            StorageLookupObservation {
                observed_at_ms: now,
                misses: BTreeSet::new(),
            },
        );
        Ok(())
    }

    /// Validate that a storage lookup response belongs to a local lookup round.
    ///
    /// Post: `Ok(())` proves a fresh bucket exists for `(resource, redundancy)`.
    pub(crate) fn ensure_storage_lookup_active(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        if observations.contains_key(&key) {
            Ok(())
        } else {
            Err(Error::InvalidMessage(
                "storage lookup response has no active local lookup".to_string(),
            ))
        }
    }

    /// Buffer placement misses observed by an in-flight storage lookup.
    ///
    /// Post: retained observation buckets satisfy the capacity and freshness
    /// invariants.
    /// Post: the supplied misses are appended only to a bucket previously created
    /// by [`Self::start_storage_lookup`].
    pub(crate) fn observe_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
        misses: impl IntoIterator<Item = PlacementMiss>,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut misses = misses.into_iter().peekable();
        if misses.peek().is_none() {
            return Ok(());
        }
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        let Some(observation) = observations.get_mut(&key) else {
            return Err(Error::InvalidMessage(
                "storage miss observation has no active local lookup".to_string(),
            ));
        };
        observation.observed_at_ms = now;
        observation.misses.extend(misses);
        evict_storage_lookup_observations(&mut observations, now);
        Ok(())
    }

    /// Drain fresh miss observations for a found entry.
    ///
    /// Post: returned misses come only from a bucket that survived freshness
    /// eviction at this call's observation time.
    /// Post: the bucket remains active with no buffered misses until TTL or a new
    /// lookup round removes it.
    /// Preservation: eviction before drain prevents stale owners from driving
    /// late read-repair.
    pub(crate) fn take_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<Vec<PlacementMiss>> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        let Some(observation) = observations.get_mut(&key) else {
            return Err(Error::InvalidMessage(
                "storage repair has no active local lookup".to_string(),
            ));
        };
        Ok(std::mem::take(&mut observation.misses)
            .into_iter()
            .collect())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    /// Test hook: make one observation bucket older than the freshness TTL.
    pub(crate) fn expire_storage_lookup_observation(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<()> {
        let key = self.storage_lookup_observation_key(resource, redundancy)?;
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        if let Some(observation) = observations.get_mut(&key) {
            observation.observed_at_ms = storage_lookup_observation_now_ms()
                .saturating_sub(STORAGE_LOOKUP_OBSERVATION_TTL_MS + 1);
        }
        Ok(())
    }

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    /// Test hook: count retained observation buckets.
    pub(crate) fn storage_lookup_observation_count(&self) -> Result<usize> {
        let observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        Ok(observations.len())
    }

    fn pending_peers(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, PendingPeerPool<DEFAULT_PENDING_CONNECTION_CAPACITY>>>
    {
        self.pending_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn active_peers(&self) -> Result<std::sync::MutexGuard<'_, BTreeSet<Did>>> {
        self.active_peers
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    fn get_raw_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    fn is_pending_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.pending_peers()?.contains(peer))
    }

    /// Return whether `peer` completed a handshake and still owns a logical slot.
    ///
    /// Unlike [`Self::is_active_connection`], this remains true while a terminal
    /// callback removes the peer, so lifecycle cleanup can evict it from the DHT
    /// even after WebRTC reports `Closed`.
    pub(crate) fn is_admitted_connection(&self, peer: Did) -> bool {
        self.active_peers()
            .map(|active| active.contains(&peer))
            .unwrap_or(false)
    }

    /// Return whether `peer` completed its pending handshake and can route traffic.
    ///
    /// Data-channel open is the admission boundary. WebRTC may still report a
    /// transient `Disconnected` state after admission, so only terminal states
    /// make an admitted peer non-routable before its callback removes the slot.
    pub(crate) fn is_active_connection(&self, peer: Did) -> bool {
        self.is_admitted_connection(peer)
            && self.get_raw_connection(peer).is_some_and(|connection| {
                !matches!(
                    connection.webrtc_connection_state(),
                    WebrtcConnectionState::Failed | WebrtcConnectionState::Closed
                )
            })
    }

    async fn reserve_pending_connection(&self, peer: Did) -> Result<PendingConnectionAttempt> {
        self.expire_pending_connections().await?;
        if peer == self.dht.did {
            return Err(Error::ShouldNotConnectSelf);
        }
        // A peer keeps its active slot through transient WebRTC state changes
        // until its terminal callback removes it from the DHT. Do not admit a
        // second pending handshake for that DID during this interval.
        if self.is_admitted_connection(peer) {
            return Err(Error::AlreadyConnected);
        }
        self.pending_peers()?.reserve(peer, get_epoch_ms_i64())
    }

    fn pending_attempt(&self, peer: Did) -> Result<Option<PendingConnectionAttempt>> {
        let pending = self.pending_peers()?;
        Ok(pending
            .peers
            .get(&peer)
            .map(|pending| PendingConnectionAttempt {
                peer,
                generation: pending.generation,
            }))
    }

    pub(crate) fn promote_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        let mut pending = self.pending_peers()?;
        if !pending.remove(attempt) {
            return Ok(false);
        }
        drop(pending);
        self.active_peers()?.insert(attempt.peer);
        Ok(true)
    }

    fn retire_pending_connection(&self, attempt: PendingConnectionAttempt) -> Result<bool> {
        Ok(self.pending_peers()?.remove(attempt))
    }

    fn retire_active_connection(&self, peer: Did) -> Result<bool> {
        Ok(self.active_peers()?.remove(&peer))
    }

    /// Cancel a current pending handshake and release its non-routable transport object.
    pub(crate) async fn cancel_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Result<bool> {
        if !self.retire_pending_connection(attempt)? {
            return Ok(false);
        }
        self.transport
            .close_connection(&attempt.peer.to_string())
            .await
            .map_err(Error::Transport)?;
        Ok(true)
    }

    async fn abandon_pending_connection(&self, attempt: PendingConnectionAttempt, operation: &str) {
        if let Err(error) = self.cancel_pending_connection(attempt).await {
            tracing::warn!(
                "failed to cancel pending connection to {} after {operation}: {error}",
                attempt.peer
            );
        }
    }

    /// Close pending handshakes whose data channel did not open before the deadline.
    ///
    /// These peers have never entered the DHT, so expiry only releases the
    /// transport object; it deliberately performs no topology mutation.
    pub(crate) async fn expire_pending_connections(&self) -> Result<()> {
        let expired = self.pending_peers()?.expire(get_epoch_ms_i64());
        for attempt in expired {
            tracing::warn!("pending connection to {} timed out", attempt.peer);
            self.transport
                .close_connection(&attempt.peer.to_string())
                .await
                .map_err(Error::Transport)?;
        }
        Ok(())
    }

    /// Create a new non-routable transport connection and register its pending attempt.
    async fn new_pending_connection(
        &self,
        attempt: PendingConnectionAttempt,
        callback: InnerSwarmCallback,
    ) -> Result<()> {
        let cid = attempt.peer.to_string();
        if let Err(error) = self
            .transport
            .new_connection(&cid, Box::new(callback))
            .await
        {
            let _ = self.retire_pending_connection(attempt);
            return Err(Error::Transport(error));
        }
        Ok(())
    }

    /// Get an active, routable connection by DID.
    ///
    /// Pending and terminal physical transports are intentionally invisible here.
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
            .map(|active| active.iter().copied().collect())
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

    #[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
    pub(crate) fn pending_connection_count(&self) -> Result<usize> {
        Ok(self.pending_peers()?.len())
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
                self.record_peer_message_send_failed(peer).await;
                return Err(e);
            }
        };
        if let Err(error) = self
            .send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await
        {
            self.abandon_pending_connection(attempt, "sending connection offer")
                .await;
            return Err(error);
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
            let timeout = Delay::new(wait_timeout).fuse();
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

        let offer = match conn.connection.webrtc_create_offer().await {
            Ok(offer) => offer,
            Err(error) => {
                self.abandon_pending_connection(attempt, "creating connection offer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
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

        let offer = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;

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

        let answer = match conn.connection.webrtc_answer_offer(offer).await {
            Ok(answer) => answer,
            Err(error) => {
                self.abandon_pending_connection(attempt, "creating connection answer")
                    .await;
                return Err(Error::Transport(error));
            }
        };
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

        let answer = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        if !self.is_pending_connection(peer)? {
            return Err(Error::SwarmMissTransport(peer));
        }

        let conn = self
            .get_raw_connection(peer)
            .ok_or(Error::SwarmMissTransport(peer))?;
        if let Err(error) = conn.connection.webrtc_accept_answer(answer).await {
            let attempt = self.pending_attempt(peer)?;
            if let Some(attempt) = attempt {
                self.abandon_pending_connection(attempt, "accepting connection answer")
                    .await;
            }
            return Err(Error::Transport(error));
        }

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

        tracing::debug!(
            "Try send {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        let data = payload.to_bincode()?;
        if data.len() > TRANSPORT_MAX_SIZE {
            tracing::error!("Message is too large: {:?}", payload);
            return Err(Error::MessageTooLarge(data.len()));
        }

        // The chunk-vs-whole decision is the pure `WireReserves::plan`, against this connection's
        // negotiated `max_message_size`; this block is only the effectful shell carrying it out.
        // `None` means the peer's limit is too small to carry even one useful chunk — a real failure
        // we surface (before sending anything) rather than fragmenting into a flood of near-empty
        // chunks. Both arms are **fire-and-forget**: `send_message` returns once the bytes are
        // accepted into the send buffer, not once they flush — a whole message hands its
        // `DeliveryFuture` to the runtime, and a chunked message is driven by one bounded background
        // task (one chunk in flight; see `run_chunked_send`), so a large payload never blocks the
        // caller's path while keeping memory and the runtime task count bounded.
        let Some(plan) = WireReserves::PRODUCTION.plan(data.len(), conn.max_message_size()) else {
            self.record_peer_message_send_failed(did).await;
            return Err(Error::PeerMaxMessageSizeTooSmall(conn.max_message_size()));
        };
        match plan {
            Framing::Whole => match conn.send_data(data).await {
                Ok(delivery) => spawn_delivery(delivery, did, self.measure.clone()),
                Err(e) => {
                    self.record_peer_message_send_failed(did).await;
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
                    match conn.send_data(first).await {
                        Ok(first_delivery) => {
                            spawn_chunked_send(
                                conn,
                                Box::new(chunks),
                                first_delivery,
                                self.session_sk.clone(),
                                did,
                                self.measure.clone(),
                            );
                        }
                        Err(e) => {
                            self.record_peer_message_send_failed(did).await;
                            return Err(e);
                        }
                    }
                }
            }
        }

        tracing::debug!(
            "Sent {:?}, to node {:?}",
            payload.clone(),
            payload.relay.next_hop,
        );

        Ok(())
    }
}

#[cfg_attr(all(feature = "wasm", target_family = "wasm"), async_trait(?Send))]
#[cfg_attr(not(all(feature = "wasm", target_family = "wasm")), async_trait)]
impl LiveDid for SwarmConnection {
    async fn live(&self) -> bool {
        self.webrtc_connection_state() == WebrtcConnectionState::Connected
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}

#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
mod tests;
