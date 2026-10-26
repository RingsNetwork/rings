use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
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
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::LaneCreditReservation;
use rings_transport::core::transport::LaneCreditView;
use rings_transport::core::transport::SendPermit;
use rings_transport::core::transport::WebrtcConnectionState;
use rings_transport::delivery::DeliveryFuture;
use rings_transport::webrtc_config::WebrtcUdpPortRange;

use self::storage_sync::StorageSyncAckMap;
use crate::chunk::ReassemblyLimits;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDeliveryCursor;
use crate::dht::VirtualNodeConfig;
use crate::error::Error;
use crate::error::Result;
use crate::measure::MeasureImpl;
use crate::measure::MeasurementEvent;
use crate::message::ConnectNodeReport;
use crate::message::ConnectNodeSend;
use crate::message::DhtProtocolMode;
use crate::message::TransactionReplay;
use crate::swarm::application::ApplicationPort;
use crate::swarm::observer::MessageObservation;
use crate::swarm::observer::SharedSwarmObserver;
use crate::utils::get_epoch_ms_i64;

mod connection;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod deadline_hold;
mod delivery;
pub(crate) mod egress;
#[cfg(test)]
mod frame_ledger;
mod handshake;
#[cfg(test)]
mod inbound_tap;
mod link_control;
mod liveness;
mod measurement;
mod outbound;
mod payload_send;
mod peer_operation;
mod pending;
mod readiness;
mod rerouting;
mod retention;
mod storage_lookup;
mod storage_sync;
pub(crate) mod store_reports;
mod transaction_replay;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use storage_sync::StorageSyncBatch;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use storage_sync::StorageSyncBatchStep;
mod timeouts;
pub(crate) use self::connection::AdmittedConnection;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::delivery::delivery_stall_deadline_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::delivery::hold_frame_deadlines_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::delivery::SendCompletionOutcome;
#[cfg(test)]
pub(crate) use self::frame_ledger::FrameInFlight;
#[cfg(test)]
use self::frame_ledger::FrameLedger;
#[cfg(test)]
pub(crate) use self::frame_ledger::FrameSample;
pub(crate) use self::handshake::CounterOffer;
pub(crate) use self::handshake::CreatedAnswer;
pub use self::handshake::OfferReply;
#[cfg(test)]
pub(crate) use self::inbound_tap::InboundTap;
pub(crate) use self::liveness::IssuedProbe;
use self::liveness::PeerLivenessMap;
pub(crate) use self::liveness::ProbePass;
pub(crate) use self::liveness::ProbeScope;
pub(crate) use self::liveness::ProbeWork;
pub(crate) use self::liveness::CHAIN_NEIGHBOUR_LIVENESS_PERIOD_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::liveness::CHAIN_NEIGHBOUR_SUSPICION_MS;
#[cfg(all(test, feature = "dummy"))]
pub(crate) use self::liveness::PEER_LIVENESS_PERIOD_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::liveness::PEER_LIVENESS_TIMEOUT_MS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::dispatched_link_control_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::dispatched_link_control_since;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::outbound_submit_count_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::referenced_slots_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::reset_outbound_submit_count_for_test;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::test_trace::LinkDirection;
use self::outbound::OutboundSchedulers;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::LINK_CONTROL_IN_FLIGHT_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_CONTROL_BURST;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_CONTROL_RESERVED_TRANSFERS;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_DATA_TRANSFER_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_GLOBAL_BYTE_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_LANE_WINDOW;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::outbound::OUTBOUND_TRANSFER_QUEUE_CAPACITY;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::payload_send::hold_first_frame_deadlines_for_test;
use self::peer_operation::PeerOperationLocks;
use self::pending::ConnectionLifecycleBoundary;
pub(crate) use self::pending::OfferOwner;
pub(crate) use self::pending::PendingConnectionAttempt;
use self::pending::PendingFingerUpdates;
use self::pending::SharedConnectionLifecycles;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
use self::pending::PENDING_CONNECTION_TIMEOUT_MS;
pub(crate) use self::readiness::TransportReadiness;
pub(crate) use self::rerouting::reroute;
pub(crate) use self::rerouting::Attempts;
pub(crate) use self::rerouting::Placement;
pub(crate) use self::rerouting::Route;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
pub(crate) use self::retention::UNREFERENCED_CONNECTION_GRACE_MS;
use self::storage_lookup::StorageAnswerClock;
use self::storage_lookup::StorageLookupObservationMap;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
pub(crate) use self::storage_lookup::STORAGE_LOOKUP_OBSERVATION_CAPACITY;
pub(crate) use self::storage_sync::StorageSyncOutcome;
pub(crate) use self::timeouts::DATA_CHANNEL_SEND_ACCEPT_BUDGET;
pub(crate) use self::timeouts::DELIVERY_BOUND;
pub(crate) use self::timeouts::TRACKED_PAYLOAD_COMPLETION_BOUND;
use self::timeouts::TRANSPORT_TIMEOUT_PROFILE;
use super::callback::InboundCapacity;

/// Everything a [`SwarmTransport`] is constructed from.
pub(crate) struct SwarmTransportParts {
    /// Overlay whose peers this transport admits and signs for.
    pub(crate) network_id: u32,
    /// WebRTC ICE and address configuration.
    pub(crate) webrtc: SwarmWebrtcConfig,
    /// Delegation key that signs every payload.
    pub(crate) delegatee_key: DelegateeKey,
    /// Local DHT the transport keeps in step with its admitted peers.
    pub(crate) dht: Arc<PeerRing>,
    /// Optional measurement sink.
    pub(crate) measure: Option<MeasureImpl>,
    /// Destination-scoped replay protection.
    pub(crate) transaction_replay: Arc<TransactionReplay>,
    /// Storage, virtual-node and reassembly settings.
    pub(crate) settings: SwarmTransportSettings,
    /// Bounded synchronous sink for privacy-safe operational observations.
    pub(crate) observer: SharedSwarmObserver,
}

pub struct SwarmTransport {
    pub(crate) network_id: u32,
    transport: Transport,
    delegatee_key: DelegateeKey,
    pub(crate) dht: Arc<PeerRing>,
    storage_redundancy: u16,
    dht_virtual_nodes: u16,
    reassembly_limits: ReassemblyLimits,
    inbound_capacity: Arc<InboundCapacity>,
    connection_lifecycle: ConnectionLifecycleBoundary,
    /// Core's side of the application boundary (#935).
    application: ApplicationPort,
    connection_creation: PeerOperationLocks,
    peer_lifecycles: SharedConnectionLifecycles,
    pending_finger_updates: Mutex<PendingFingerUpdates>,
    peer_liveness: Mutex<PeerLivenessMap>,
    /// The exclusive right to run a probe pass; see [`ProbePass`].
    probe_pass: FuturesMutex<()>,
    storage_lookup_observations: Mutex<StorageLookupObservationMap>,
    /// The reply marker of storage lookups; see [`StorageAnswerClock`].
    storage_answer_clock: StorageAnswerClock,
    pending_storage_sync_acks: Mutex<StorageSyncAckMap>,
    /// The store reports this node awaits for the reported operations it sent (#866).
    pub(crate) store_reports: self::store_reports::StoreReports,
    storage_repair_requested: AtomicBool,
    /// Test builds: the frames this node sent and received; see [`FrameLedger`].
    #[cfg(test)]
    frames: Arc<FrameLedger>,
    /// Test builds: every payload entering the logical inbound stage; see [`InboundTap`].
    #[cfg(test)]
    inbound_tap: InboundTap,
    storage_repair_cursor: Mutex<Option<StorageSyncDeliveryCursor>>,
    outbound_schedulers: OutboundSchedulers,
    measured_disconnects: Mutex<MeasuredDisconnectMap>,
    measure: Option<MeasureImpl>,
    transaction_replay: Arc<TransactionReplay>,
    /// Synchronous bounded sink for semantic message and lookup activity.
    observer: SharedSwarmObserver,
}

type MeasuredDisconnectMap = BTreeMap<Did, MeasuredDisconnect>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MeasuredDisconnect {
    generation: u64,
    disconnected_since_ms: i64,
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
    /// Test builds: the owning transport's frame counts, charged by [`Self::send_data`].
    #[cfg(test)]
    frames: Arc<FrameLedger>,
}

impl SwarmTransport {
    fn measured_disconnects(&self) -> Result<MutexGuard<'_, MeasuredDisconnectMap>> {
        self.measured_disconnects
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    /// Build the transport from its parts; see [`SwarmTransportParts`].
    pub(crate) fn new(parts: SwarmTransportParts) -> Self {
        let SwarmTransportParts {
            network_id,
            webrtc,
            delegatee_key,
            dht,
            measure,
            transaction_replay,
            settings,
            observer,
        } = parts;
        let lifecycle_bounds = self::retention::lifecycle_bounds(dht.successors().capacity());
        Self {
            network_id,
            transport: Transport::new(
                &webrtc.ice_servers,
                webrtc.external_address,
                webrtc.udp_port_range,
            ),
            delegatee_key,
            dht,
            storage_redundancy: settings.storage_redundancy,
            dht_virtual_nodes: settings.dht_virtual_nodes,
            reassembly_limits: settings.reassembly_limits,
            inbound_capacity: Arc::new(InboundCapacity::new()),
            connection_lifecycle: ConnectionLifecycleBoundary::new(),
            application: ApplicationPort::new(),
            connection_creation: PeerOperationLocks::new(),
            peer_lifecycles: Arc::new(Mutex::new(self::pending::ConnectionLifecycleRegistry::new(
                lifecycle_bounds,
            ))),
            pending_finger_updates: Mutex::new(BTreeMap::new()),
            peer_liveness: Mutex::new(PeerLivenessMap::new()),
            probe_pass: FuturesMutex::new(()),
            storage_lookup_observations: Mutex::new(BTreeMap::new()),
            storage_answer_clock: StorageAnswerClock::default(),
            pending_storage_sync_acks: Mutex::new(BTreeMap::new()),
            store_reports: self::store_reports::StoreReports::default(),
            storage_repair_requested: AtomicBool::new(false),
            #[cfg(test)]
            frames: Arc::default(),
            #[cfg(test)]
            inbound_tap: InboundTap::default(),
            storage_repair_cursor: Mutex::new(None),
            outbound_schedulers: OutboundSchedulers::new(measure.clone()),
            measured_disconnects: Mutex::new(BTreeMap::new()),
            measure,
            transaction_replay,
            observer,
        }
    }

    /// Publish one already-sanitized message observation without blocking protocol progress.
    pub(crate) fn observe_message(&self, observation: MessageObservation) {
        self.observer.observe_message(observation);
    }

    /// Borrow the configured operational observer for lookup lifecycle reporting.
    pub(crate) fn observer(&self) -> &SharedSwarmObserver {
        &self.observer
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
            .map_err(|_| Error::LockPoisoned)?;
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
            .map_err(|_| Error::LockPoisoned)?;
        *cursor = Some(scheduled);
        Ok(())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn storage_repair_cursor_for_simulation(&self) -> Result<Option<String>> {
        let cursor = self
            .storage_repair_cursor
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(cursor.as_ref().map(|cursor| format!("{cursor:?}")))
    }

    /// The session this node authenticates with: its public half and account authorization.
    pub(crate) fn delegation(&self) -> Delegation {
        self.delegatee_key.delegation()
    }

    /// Storage virtual-node positions required by this DHT protocol mode.
    pub(crate) fn dht_virtual_nodes(&self) -> u16 {
        self.dht_virtual_nodes
    }

    pub(crate) fn dht_protocol_mode(&self) -> DhtProtocolMode {
        DhtProtocolMode::new(
            self.network_id,
            self.storage_redundancy,
            self.dht_virtual_nodes,
        )
    }

    /// Return whether an inbound connection offer matches this DHT protocol mode.
    pub(crate) fn accepts_connection_offer(&self, offer: &ConnectNodeSend) -> bool {
        offer.dht_protocol_mode.matches(self.dht_protocol_mode())
    }

    /// Return whether an inbound connection answer matches this DHT protocol mode.
    pub(crate) fn accepts_connection_answer(&self, answer: &ConnectNodeReport) -> bool {
        answer.dht_protocol_mode.matches(self.dht_protocol_mode())
    }

    /// Chunk reassembly limits enforced by inbound callbacks.
    pub(crate) fn reassembly_limits(&self) -> ReassemblyLimits {
        self.reassembly_limits
    }

    pub(crate) fn inbound_capacity(&self) -> Arc<InboundCapacity> {
        self.inbound_capacity.clone()
    }

    /// Test builds: the frames this node sent and received; see [`FrameLedger`].
    #[cfg(test)]
    pub(crate) fn frames_for_test(&self) -> &Arc<FrameLedger> {
        &self.frames
    }

    /// Test builds: every payload entering the logical inbound stage; see [`InboundTap`].
    #[cfg(test)]
    pub(crate) fn inbound_tap_for_test(&self) -> &InboundTap {
        &self.inbound_tap
    }

    /// Test builds: the inbound events admitted and not yet handled.
    #[cfg(test)]
    pub(crate) fn inbound_live_events_for_test(&self) -> usize {
        self.inbound_capacity.live_events_for_test()
    }

    /// Core's side of the application boundary: the inbox it writes, the lookups it answers
    /// and the delegations it reads.
    pub(crate) fn application(&self) -> &ApplicationPort {
        &self.application
    }

    /// Record that `peer` reached an open data channel.
    pub(crate) async fn record_peer_connected(&self, attempt: PendingConnectionAttempt) {
        if !self.is_active_connection_attempt(attempt) {
            return;
        }
        self.mark_peer_liveness_connected(attempt);
        let updated = self.with_active_slot(attempt, || {
            self.measured_disconnects()?.remove(&attempt.peer);
            Ok(())
        });
        match updated {
            Ok(Some(())) => {}
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    "Failed to update disconnect epoch for connected peer {} generation {}: {error}",
                    attempt.peer,
                    attempt.generation
                );
                return;
            }
        }
        self.record_peer_measurement(
            attempt.peer,
            crate::measure::Authentication::Authenticated,
            MeasurementEvent::Connected,
        )
        .await;
    }

    /// Record that `peer` left the usable connection epoch.
    ///
    /// Invariant: for one connection epoch, at most one `Disconnected` counter is
    /// recorded. `record_peer_connected` starts a new epoch by clearing the marker.
    pub(crate) async fn record_peer_disconnected(&self, attempt: PendingConnectionAttempt) {
        let now_ms = get_epoch_ms_i64();
        let should_record = match self.with_active_slot(attempt, || {
            let previous = self
                .measured_disconnects()?
                .insert(attempt.peer, MeasuredDisconnect {
                    generation: attempt.generation,
                    disconnected_since_ms: now_ms,
                });
            Ok(!previous.is_some_and(|disconnect| disconnect.generation == attempt.generation))
        }) {
            Ok(Some(should_record)) => should_record,
            Ok(None) => false,
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
            self.record_peer_measurement(
                attempt.peer,
                crate::measure::Authentication::Authenticated,
                MeasurementEvent::Disconnected,
            )
            .await;
        }
    }

    /// Return the first time this peer left the usable connection epoch.
    pub(crate) fn peer_disconnected_since_attempt_ms(
        &self,
        attempt: PendingConnectionAttempt,
    ) -> Option<i64> {
        self.with_active_slot(attempt, || {
            Ok(self
                .measured_disconnects()?
                .get(&attempt.peer)
                .filter(|disconnect| disconnect.generation == attempt.generation)
                .map(|disconnect| disconnect.disconnected_since_ms))
        })
        .ok()
        .flatten()
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
        if let Err(error) = self.with_active_slot(attempt, || {
            self.measured_disconnects()?.remove(&attempt.peer);
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
        self.with_active_slot(attempt, || {
            self.measured_disconnects()?
                .insert(peer, MeasuredDisconnect {
                    generation: attempt.generation,
                    disconnected_since_ms,
                });
            Ok(())
        })
        .map(|_| ())
    }

    /// Validate that a runtime storage message uses this transport's redundancy.
    pub(crate) fn ensure_storage_redundancy(&self, redundancy: u16) -> Result<()> {
        if self.storage_redundancy == redundancy {
            Ok(())
        } else {
            Err(Error::StorageRedundancyMismatch {
                configured: self.storage_redundancy,
                requested: redundancy,
            })
        }
    }
}

impl SwarmConnection {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn dummy_generation_id(&self) -> Result<String> {
        self.connection.dummy_generation_id().map_err(Into::into)
    }

    /// Wait for one credit to send a frame on `lane`: the receiver's backpressure, untimed (see
    /// `rings_transport::core::credit`). The wait owns the lane's credit state only, so it
    /// keeps no connection alive however long it lasts.
    fn reserve_send_credit(
        &self,
        lane: ChannelLane,
    ) -> impl std::future::Future<Output = Result<LaneCreditReservation>> + 'static {
        let wait = self.connection.reserve_send_credit(lane);
        async move { wait.await.map_err(Into::into) }
    }

    /// A read-only view of `lane`'s send credit on this connection generation; it holds the
    /// connection weakly and reserves nothing.
    pub(crate) fn send_credit_view(&self, lane: ChannelLane) -> LaneCreditView {
        self.connection.send_credit_view(lane)
    }

    /// Hand one frame to the transport on `lane`; the sole send of every frame to another node.
    async fn send_data(
        &self,
        data: Bytes,
        lane: ChannelLane,
        permit: SendPermit,
    ) -> Result<DeliveryFuture> {
        // Test builds: a send dropped before the transport committed to it ends uncounted.
        #[cfg(test)]
        let frame_send = self.frames.begin_send(permit.acceptance());
        let delivery: Result<DeliveryFuture> = self
            .connection
            .send_message_with_permit(data, lane, permit)
            .await
            .map_err(Into::into);
        #[cfg(test)]
        if delivery.is_ok() {
            frame_send.accept();
        }
        delivery
    }

    async fn close(&self) -> Result<()> {
        self.connection.close().await.map_err(Into::into)
    }

    pub fn webrtc_connection_state(&self) -> WebrtcConnectionState {
        self.connection.webrtc_connection_state()
    }

    #[cfg(all(
        test,
        not(all(feature = "wasm", target_family = "wasm")),
        not(feature = "dummy")
    ))]
    pub(crate) fn physical_close_witness(
        &self,
    ) -> Result<rings_transport::connections::NativePhysicalCloseWitness> {
        self.connection.physical_close_witness().map_err(Into::into)
    }

    /// The largest single data-channel message this connection can carry — the negotiated
    /// `max_message_size`. Used to size payload chunks so each wrapped chunk stays within the limit.
    pub fn max_message_size(&self) -> usize {
        self.connection.max_message_size()
    }
}

impl From<SwarmConnection> for Did {
    fn from(conn: SwarmConnection) -> Self {
        conn.peer
    }
}

/// Stage 6 model check (#772): rejoin races across connection generations.
/// Built for every test target, so the browser build checks the same model.
#[cfg(test)]
mod test_model_check;
#[cfg(test)]
mod test_rejoin_model;
#[cfg(all(test, not(all(feature = "wasm", target_family = "wasm"))))]
mod tests;
