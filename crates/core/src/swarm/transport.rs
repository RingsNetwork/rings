use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use rings_transport::connection_ref::ConnectionRef;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyConnection as ConnectionOwner;
#[cfg(feature = "dummy")]
pub use rings_transport::connections::DummyTransport as Transport;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcConnection as ConnectionOwner;
#[cfg(feature = "wasm")]
pub use rings_transport::connections::WebSysWebrtcTransport as Transport;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcConnection as ConnectionOwner;
#[cfg(all(not(feature = "wasm"), not(feature = "dummy")))]
use rings_transport::connections::WebrtcTransport as Transport;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::TransportInterface;
use rings_transport::core::transport::TransportMessage;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;

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
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::Message;
use crate::message::MessagePayload;
use crate::message::PayloadSender;
use crate::session::SessionSk;
use crate::swarm::callback::InnerSwarmCallback;

const STORAGE_LOOKUP_OBSERVATION_TTL_MS: i64 = 30_000;
/// Maximum number of read-repair miss observation buckets retained per transport.
pub(crate) const STORAGE_LOOKUP_OBSERVATION_CAPACITY: usize = 1024;

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
    reassembly_limits: ReassemblyLimits,
    storage_lookup_observations: Mutex<StorageLookupObservationMap>,
    #[allow(dead_code)]
    measure: Option<MeasureImpl>,
}

/// Runtime limits used by [`SwarmTransport`].
#[derive(Clone, Copy)]
pub(crate) struct SwarmTransportSettings {
    storage_redundancy: u16,
    reassembly_limits: ReassemblyLimits,
}

impl SwarmTransportSettings {
    /// Build transport settings from storage repair redundancy and chunk reassembly limits.
    pub(crate) fn new(storage_redundancy: u16, reassembly_limits: ReassemblyLimits) -> Self {
        Self {
            storage_redundancy,
            reassembly_limits,
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
    Utc::now().timestamp_millis()
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
        let Some(stale_key) = observations
            .iter()
            .min_by_key(|(_, observation)| observation.observed_at_ms)
            .map(|(key, _)| *key)
        else {
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

/// Drive a message's [DeliveryFuture] to completion on the runtime, logging if
/// the message was lost before it could be flushed. This keeps delivery
/// tracking confined to the send site: the status never propagates up through
/// the swarm/node layers.
#[cfg(feature = "wasm")]
fn spawn_delivery(fut: DeliveryFuture, did: Did) {
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = fut.await {
            tracing::warn!("Message to {did} was not delivered: {e}");
        }
    });
}

/// Drive a message's [DeliveryFuture] to completion on the runtime, logging if
/// the message was lost before it could be flushed.
#[cfg(not(feature = "wasm"))]
fn spawn_delivery(fut: DeliveryFuture, did: Did) {
    tokio::spawn(async move {
        if let Err(e) = fut.await {
            tracing::warn!("Message to {did} was not delivered: {e}");
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
#[cfg(not(feature = "wasm"))]
type ChunkTail = Box<dyn Iterator<Item = Chunk> + Send>;
#[cfg(feature = "wasm")]
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
) {
    if let Err(e) = first_delivery.await {
        tracing::warn!("Chunked send to {did} stopped before the first chunk flushed: {e}");
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
                    return;
                }
            }
            Err(e) => {
                tracing::warn!("Chunked send to {did} stopped: {e}");
                return;
            }
        }
    }
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(feature = "wasm")]
fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
) {
    wasm_bindgen_futures::spawn_local(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
    ));
}

/// Drive the tail of a chunked send on the runtime (one bounded task per large message). See
/// [`run_chunked_send`].
#[cfg(not(feature = "wasm"))]
fn spawn_chunked_send(
    conn: SwarmConnection,
    tail: ChunkTail,
    first_delivery: DeliveryFuture,
    session_sk: SessionSk,
    did: Did,
) {
    tokio::spawn(run_chunked_send(
        conn,
        tail,
        first_delivery,
        session_sk,
        did,
    ));
}

impl SwarmTransport {
    pub(crate) fn new(
        network_id: u32,
        ice_servers: &str,
        external_address: Option<String>,
        session_sk: SessionSk,
        dht: Arc<PeerRing>,
        measure: Option<MeasureImpl>,
        settings: SwarmTransportSettings,
    ) -> Self {
        Self {
            network_id,
            transport: Transport::new(ice_servers, external_address),
            session_sk,
            dht,
            storage_redundancy: settings.storage_redundancy,
            reassembly_limits: settings.reassembly_limits,
            storage_lookup_observations: Mutex::new(BTreeMap::new()),
            measure,
        }
    }

    /// Redundancy used by storage repair and anti-entropy.
    pub(crate) fn storage_redundancy(&self) -> u16 {
        self.storage_redundancy
    }

    /// Chunk reassembly limits enforced by inbound callbacks.
    pub(crate) fn reassembly_limits(&self) -> ReassemblyLimits {
        self.reassembly_limits
    }

    /// Ensure the storage API redundancy matches repair redundancy.
    pub(crate) fn ensure_storage_redundancy<const REDUNDANT: u16>(&self) -> Result<()> {
        if self.storage_redundancy == REDUNDANT {
            Ok(())
        } else {
            Err(Error::StorageRedundancyMismatch {
                configured: self.storage_redundancy,
                requested: REDUNDANT,
            })
        }
    }

    /// Start a fresh lookup round for `resource`.
    ///
    /// This removes any previous miss observations for the same resource and
    /// redundancy so targeted read-repair never drains misses from an older
    /// lookup round.
    ///
    /// Post: no bucket exists for `(resource, redundancy)`.
    /// Preservation: eviction establishes the capacity and freshness invariants
    /// before removing the lookup-round bucket.
    pub(crate) fn start_storage_lookup(&self, resource: Did, redundancy: u16) -> Result<()> {
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        observations.remove(&StorageLookupObservationKey {
            resource,
            redundancy,
        });
        Ok(())
    }

    /// Buffer placement misses observed by an in-flight storage lookup.
    ///
    /// Post: retained observation buckets satisfy the capacity and freshness
    /// invariants.
    /// Post: if the `(resource, redundancy)` bucket survives capacity eviction,
    /// it contains the supplied misses and its freshness witness is this call's
    /// observation time.
    pub(crate) fn observe_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
        misses: impl IntoIterator<Item = PlacementMiss>,
    ) -> Result<()> {
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
        let key = StorageLookupObservationKey {
            resource,
            redundancy,
        };
        let observation = observations
            .entry(key)
            .or_insert_with(|| StorageLookupObservation {
                observed_at_ms: now,
                misses: BTreeSet::new(),
            });
        observation.observed_at_ms = now;
        observation.misses.extend(misses);
        evict_storage_lookup_observations(&mut observations, now);
        Ok(())
    }

    /// Drain fresh miss observations for a found entry.
    ///
    /// Post: returned misses come only from a bucket that survived freshness
    /// eviction at this call's observation time.
    /// Post: no bucket remains for `(resource, redundancy)`.
    /// Preservation: eviction before drain prevents stale owners from driving
    /// late read-repair.
    pub(crate) fn take_storage_misses(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<Vec<PlacementMiss>> {
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        let now = storage_lookup_observation_now_ms();
        evict_storage_lookup_observations(&mut observations, now);
        let key = StorageLookupObservationKey {
            resource,
            redundancy,
        };
        Ok(observations
            .remove(&key)
            .map(|observation| observation.misses.into_iter().collect())
            .unwrap_or_default())
    }

    #[cfg(all(test, not(feature = "wasm")))]
    /// Test hook: make one observation bucket older than the freshness TTL.
    pub(crate) fn expire_storage_lookup_observation(
        &self,
        resource: Did,
        redundancy: u16,
    ) -> Result<()> {
        let mut observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        if let Some(observation) = observations.get_mut(&StorageLookupObservationKey {
            resource,
            redundancy,
        }) {
            observation.observed_at_ms = storage_lookup_observation_now_ms()
                .saturating_sub(STORAGE_LOOKUP_OBSERVATION_TTL_MS + 1);
        }
        Ok(())
    }

    #[cfg(all(test, not(feature = "wasm")))]
    /// Test hook: count retained observation buckets.
    pub(crate) fn storage_lookup_observation_count(&self) -> Result<usize> {
        let observations = self
            .storage_lookup_observations
            .lock()
            .map_err(|_| Error::DHTSyncLockError)?;
        Ok(observations.len())
    }

    /// Create new connection that will be handled by swarm.
    pub async fn new_connection(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        if peer == self.dht.did {
            return Ok(());
        }

        let cid = peer.to_string();
        self.transport
            .new_connection(&cid, Box::new(callback))
            .await
            .map_err(Error::Transport)
    }

    /// Get connection by did.
    pub fn get_connection(&self, peer: Did) -> Option<SwarmConnection> {
        self.transport
            .connection(&peer.to_string())
            .map(|conn| SwarmConnection {
                peer,
                connection: conn,
            })
            .ok()
    }

    /// Get all connections in transport.
    pub fn get_connections(&self) -> Vec<(Did, SwarmConnection)> {
        self.transport
            .connections()
            .into_iter()
            .filter_map(|(k, v)| {
                Did::from_str(&k).ok().map(|did| {
                    (did, SwarmConnection {
                        peer: did,
                        connection: v,
                    })
                })
            })
            .collect()
    }

    /// Get dids of all connections in transport.
    pub fn get_connection_ids(&self) -> Vec<Did> {
        self.transport
            .connection_ids()
            .into_iter()
            .filter_map(|k| Did::from_str(&k).ok())
            .collect()
    }

    /// Disconnect a connection. There are three steps:
    /// 1) remove from DHT;
    /// 2) remove from Transport;
    /// 3) close the connection;
    pub async fn disconnect(&self, peer: Did) -> Result<()> {
        tracing::info!("removing {peer} from DHT");
        self.dht.remove(peer)?;
        self.transport
            .close_connection(&peer.to_string())
            .await
            .map_err(|e| e.into())
    }

    /// Connect a given Did. If the did is already connected, return Err,
    /// else try prepare offer and establish connection by dht.
    pub async fn connect(&self, peer: Did, callback: InnerSwarmCallback) -> Result<()> {
        let offer_msg = self.prepare_connection_offer(peer, callback).await?;
        self.send_message(Message::ConnectNodeSend(offer_msg), peer)
            .await?;
        Ok(())
    }

    /// Get connection by did and check if data channel is open.
    /// This method will return None if the connection is not found.
    /// This method will wait_for_data_channel_open.
    /// If it's not ready in 8 seconds this method will close it and return None.
    /// If it's ready in 8 seconds this method will return the connection.
    /// See more information about [rings_transport::core::transport::WebrtcConnectionState].
    /// See also method webrtc_wait_for_data_channel_open [rings_transport::core::transport::ConnectionInterface].
    pub async fn get_and_check_connection(&self, peer: Did) -> Option<SwarmConnection> {
        let conn = self.get_connection(peer)?;

        if let Err(e) = conn.connection.webrtc_wait_for_data_channel_open().await {
            tracing::warn!(
                "[get_and_check_connection] connection {peer} data channel not open, will be dropped, reason: {e:?}"
            );

            if let Err(e) = self.disconnect(peer).await {
                tracing::error!("Failed on close connection {peer}: {e:?}");
            }

            return None;
        };

        Some(conn)
    }

    /// Create new connection and its offer.
    pub async fn prepare_connection_offer(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
    ) -> Result<ConnectNodeSend> {
        if self.get_and_check_connection(peer).await.is_some() {
            return Err(Error::AlreadyConnected);
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let offer = conn.webrtc_create_offer().await.map_err(Error::Transport)?;
        let offer_str = serde_json::to_string(&offer).map_err(|_| Error::SerializeToString)?;
        let offer_msg = ConnectNodeSend {
            sdp: offer_str,
            network_id: self.network_id,
        };

        Ok(offer_msg)
    }

    /// Answer the offer of remote connection.
    pub async fn answer_remote_connection(
        &self,
        peer: Did,
        callback: InnerSwarmCallback,
        offer_msg: &ConnectNodeSend,
    ) -> Result<ConnectNodeReport> {
        let offer = serde_json::from_str(&offer_msg.sdp).map_err(Error::Deserialize)?;

        if let Some(swarm_conn) = self.get_connection(peer) {
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
                    self.disconnect(peer).await?;
                } else {
                    // ignore remote offer, and refuse to answer remote offer
                    return Err(Error::AlreadyConnected);
                }
            } else if self.get_and_check_connection(peer).await.is_some() {
                return Err(Error::AlreadyConnected);
            };
        };

        self.new_connection(peer, callback).await?;
        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;

        let answer = conn
            .webrtc_answer_offer(offer)
            .await
            .map_err(Error::Transport)?;
        let answer_str = serde_json::to_string(&answer).map_err(|_| Error::SerializeToString)?;
        let answer_msg = ConnectNodeReport { sdp: answer_str };

        Ok(answer_msg)
    }

    /// Accept the answer of remote connection.
    pub async fn accept_remote_connection(
        &self,
        peer: Did,
        answer_msg: &ConnectNodeReport,
    ) -> Result<()> {
        let answer = serde_json::from_str(&answer_msg.sdp).map_err(Error::Deserialize)?;

        let conn = self
            .transport
            .connection(&peer.to_string())
            .map_err(Error::Transport)?;
        conn.webrtc_accept_answer(answer)
            .await
            .map_err(Error::Transport)?;

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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
        let conn = self
            .get_and_check_connection(did)
            .await
            .ok_or(Error::SwarmMissDidInTable(did))?;

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
        let plan = WireReserves::PRODUCTION
            .plan(data.len(), conn.max_message_size())
            .ok_or(Error::PeerMaxMessageSizeTooSmall(conn.max_message_size()))?;
        match plan {
            Framing::Whole => spawn_delivery(conn.send_data(data).await?, did),
            Framing::Chunked { chunk_size } => {
                // Frame and accept the FIRST chunk on the caller's path, so an immediate send
                // failure (the buffer rejecting the bytes) surfaces here exactly as it does for a
                // whole message — `await send_message` callers learn the send was admitted. The
                // first chunk's flush and every remaining chunk are then driven by one bounded
                // background task (`run_chunked_send`), preserving fire-and-forget for the rest.
                let mut chunks = ChunkList::stream(data, chunk_size);
                if let Some(first) = chunks.next() {
                    let first = frame_chunk(&self.session_sk, did, first)?;
                    let first_delivery = conn.send_data(first).await?;
                    spawn_chunked_send(
                        conn,
                        Box::new(chunks),
                        first_delivery,
                        self.session_sk.clone(),
                        did,
                    );
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

#[cfg_attr(feature = "wasm", async_trait(?Send))]
#[cfg_attr(not(feature = "wasm"), async_trait)]
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
