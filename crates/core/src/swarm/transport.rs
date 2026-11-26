use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::RwLock;

use bytes::Bytes;
use futures::lock::Mutex as FuturesMutex;
use rings_transport::connection_ref::ConnectionRef;
use rings_transport::core::pool::ChannelLane;
use rings_transport::core::transport::ConnectionInterface;
use rings_transport::core::transport::DynConnection;
use rings_transport::core::transport::LaneCreditReservation;
use rings_transport::core::transport::LaneCreditView;
use rings_transport::core::transport::SendPermit;
use rings_transport::core::transport::SharedTransport;
use rings_transport::core::transport::TransportConnectionState;
use rings_transport::delivery::DeliveryFuture;

use self::storage_sync::StorageSyncAckMap;
use crate::chunk::ReassemblyLimits;
use crate::delegation::DelegateeKey;
use crate::delegation::Delegation;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::dht::StorageSyncDeliveryCursor;
use crate::dht::StorageSyncPurpose;
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
use crate::utils::SharedClock;
use crate::utils::SharedRng;

/// Admitted peer connection wrappers and generation-bound handles.
mod connection;
#[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
mod deadline_hold;
/// Outbound frame delivery, chunking, and stall handling.
mod delivery;
pub(crate) mod egress;
#[cfg(test)]
mod frame_ledger;
mod handshake;
#[cfg(test)]
mod inbound_tap;
mod link_control;
mod liveness;
/// Peer quality measurement and disconnect-duration tracking.
mod measurement;
mod outbound;
/// Payload wire encoding, framing, and first-frame admission watching.
mod payload_send;
mod peer_operation;
/// Pending handshake slots and generation tracking.
mod pending;
/// Connection readiness checks and transition notifications.
mod readiness;
mod rerouting;
mod retention;
/// DHT storage lookup coordination and answer clocks.
mod storage_lookup;
/// Periodic and repair DHT storage synchronization.
mod storage_sync;
pub(crate) mod store_reports;
mod transaction_replay;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use storage_sync::StorageSyncBatch;
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) use storage_sync::StorageSyncBatchStep;
/// Transport handshake and delivery timeout constants.
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
pub(crate) use self::handshake::InitialOfferStep;
pub(crate) use self::handshake::InitiatorReplyOutcome;
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
pub(crate) use self::timeouts::CHANNEL_SEND_ACCEPT_BUDGET;
pub(crate) use self::timeouts::DELIVERY_BOUND;
pub(crate) use self::timeouts::TRACKED_PAYLOAD_COMPLETION_BOUND;
use self::timeouts::TRANSPORT_TIMEOUT_PROFILE;
use super::callback::InboundCapacity;

/// Everything a [`SwarmTransport`] is constructed from.
pub(crate) struct SwarmTransportParts {
    /// Overlay whose peers this transport admits and signs for.
    pub(crate) network_id: u32,
    /// Injected trait-based transport implementation.
    pub(crate) transport: SharedTransport,
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
    /// Injectable epoch clock capability.
    pub(crate) clock: SharedClock,
    /// Injectable cryptographic RNG capability.
    pub(crate) rng: SharedRng,
}

/// Transport and connection-lifecycle coordinator backing a [`crate::swarm::Swarm`].
pub struct SwarmTransport {
    /// Network identifier separating independent overlay rings.
    pub(crate) network_id: u32,
    /// Underlying transport backend managing physical connections.
    transport: SharedTransport,
    /// Mutex serializing delegatee key rotation prepare/commit phases.
    delegatee_rotation_lock: Mutex<()>,
    /// Active delegatee signing key for this node.
    delegatee_key: RwLock<DelegateeKey>,
    /// Local Chord peer ring state and storage tables.
    pub(crate) dht: Arc<PeerRing>,
    /// Replication factor for DHT storage entries.
    storage_redundancy: u16,
    /// Number of virtual ring positions per storage owner.
    dht_virtual_nodes: u16,
    /// Memory and timeout limits for inbound chunk reassembly.
    reassembly_limits: ReassemblyLimits,
    /// Node-wide and per-peer inbound admission capacity ledger.
    inbound_capacity: Arc<InboundCapacity>,
    /// Boundary serializing connection admission, retirement, and DHT membership transitions.
    connection_lifecycle: ConnectionLifecycleBoundary,
    /// Core's side of the application boundary (#935).
    application: ApplicationPort,
    /// Per-peer locks serializing concurrent connection-creation attempts.
    connection_creation: PeerOperationLocks,
    /// Shared per-peer connection lifecycle table tracking active and pending generations.
    peer_lifecycles: SharedConnectionLifecycles,
    /// Pending finger-table update dials awaiting completion.
    pending_finger_updates: Mutex<PendingFingerUpdates>,
    /// Per-peer liveness probe and activity state.
    peer_liveness: Mutex<PeerLivenessMap>,
    /// The exclusive right to run a probe pass; see [`ProbePass`].
    probe_pass: FuturesMutex<()>,
    /// Observed storage lookup responses keyed by lookup transaction.
    storage_lookup_observations: Mutex<StorageLookupObservationMap>,
    /// The reply marker of storage lookups; see [`StorageAnswerClock`].
    storage_answer_clock: StorageAnswerClock,
    /// Pending storage synchronization acknowledgements keyed by peer and batch.
    pending_storage_sync_acks: Mutex<StorageSyncAckMap>,
    /// The store reports this node awaits for the reported operations it sent (#866).
    pub(crate) store_reports: self::store_reports::StoreReports,
    /// Flag indicating that a storage repair pass has been requested.
    storage_repair_requested: AtomicBool,
    /// Test builds: the frames this node sent and received; see [`FrameLedger`].
    #[cfg(test)]
    frames: Arc<FrameLedger>,
    /// Test builds: every payload entering the logical inbound stage; see [`InboundTap`].
    #[cfg(test)]
    inbound_tap: InboundTap,
    /// Cursor tracking progress across incremental storage repair passes.
    storage_repair_cursor: Mutex<Option<StorageSyncDeliveryCursor>>,
    /// Cooldowns suppressing redundant unchanged-topology additive storage repairs.
    storage_repair_cooldowns: Mutex<StorageRepairCooldowns>,
    /// Per-peer outbound frame schedulers.
    outbound_schedulers: OutboundSchedulers,
    /// The node's watcher of every detached send's first-frame admission.
    admission_watcher: payload_send::AdmissionWatcher,
    /// Per-peer recorded disconnect timestamps for measurement and eviction.
    measured_disconnects: Mutex<MeasuredDisconnectMap>,
    /// Exponential dial backoff state for failed candidate peers.
    candidate_dial_backoff: Mutex<CandidateDialBackoffMap>,
    /// Optional peer quality measurement backend.
    measure: Option<MeasureImpl>,
    /// Durable and volatile transaction replay deduplication engine.
    transaction_replay: Arc<TransactionReplay>,
    /// Synchronous bounded sink for semantic message and lookup activity.
    observer: SharedSwarmObserver,
    /// Injectable epoch clock capability.
    clock: SharedClock,
    /// Injectable cryptographic RNG capability.
    rng: SharedRng,
}

/// Map from peer DID to its recorded disconnect timestamp and generation.
type MeasuredDisconnectMap = BTreeMap<Did, MeasuredDisconnect>;

/// Disconnect timestamp recorded for a specific connection generation of a peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MeasuredDisconnect {
    /// Connection generation that transitioned to disconnected.
    generation: u64,
    /// Epoch timestamp in milliseconds when the disconnect was recorded.
    disconnected_since_ms: i64,
}

/// Initial backoff delay in milliseconds after a failed candidate dial.
const CANDIDATE_DIAL_BACKOFF_BASE_MS: i64 = 30_000;
/// Maximum backoff delay in milliseconds for repeated candidate dial failures.
const CANDIDATE_DIAL_BACKOFF_MAX_MS: i64 = 300_000;
/// Maximum number of peers retained in the candidate dial backoff map.
const CANDIDATE_DIAL_BACKOFF_CAPACITY: usize = 64;

/// Cooldown before resending the same steady-state [`StorageSyncPurpose::AdditiveRepair`]
/// delivery when topology has not changed.
pub(crate) const STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS: i64 = 180_000;
/// Maximum number of repair delivery cursors tracked in [`StorageRepairCooldowns`].
const STORAGE_REPAIR_COOLDOWN_MAX_ENTRIES: usize = 512;

/// Per-epoch cooldown state suppressing redundant unchanged-topology additive repairs.
#[derive(Debug, Default)]
struct StorageRepairCooldowns {
    /// Topology epoch for which `sent` timestamps were recorded.
    topology_epoch: u64,
    /// Last send timestamp (ms) keyed by destination/purpose cursor.
    sent: BTreeMap<StorageSyncDeliveryCursor, i64>,
}

/// Backoff state for one candidate peer after consecutive dial failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateDialBackoff {
    /// Number of consecutive dial failures recorded for this peer.
    failures: u32,
    /// Earliest epoch timestamp in milliseconds at which the peer may be dialed again.
    retry_after_ms: i64,
}

/// Map from candidate peer DID to its dial backoff state.
type CandidateDialBackoffMap = BTreeMap<Did, CandidateDialBackoff>;

/// Runtime settings used by [`SwarmTransport`].
#[derive(Clone, Copy)]
pub(crate) struct SwarmTransportSettings {
    /// Replication factor for DHT storage entries.
    storage_redundancy: u16,
    /// Number of virtual ring positions per storage owner.
    dht_virtual_nodes: u16,
    /// Memory and timeout limits for inbound chunk reassembly.
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

/// Read-only handle to one physical transport connection.
///
/// Clone law: clones observe the same physical connection. A
/// `SwarmConnection` does not carry logical-generation authority; sending is
/// therefore private to the generation-bound [`AdmittedConnection`] workflow.
#[derive(Clone)]
pub struct SwarmConnection {
    /// Remote peer DID associated with this physical connection.
    peer: Did,
    /// Underlying transport connection reference.
    connection: ConnectionRef<DynConnection>,
    /// Test builds: the owning transport's frame counts, charged by [`Self::send_data`].
    #[cfg(test)]
    frames: Arc<FrameLedger>,
}

impl SwarmTransport {
    /// Lock and return the map of measured per-peer disconnect timestamps.
    fn measured_disconnects(&self) -> Result<MutexGuard<'_, MeasuredDisconnectMap>> {
        self.measured_disconnects
            .lock()
            .map_err(|_| Error::SwarmConnectionLifecycleLock)
    }

    /// Build the transport from its parts; see [`SwarmTransportParts`].
    pub(crate) fn new(parts: SwarmTransportParts) -> Self {
        let SwarmTransportParts {
            network_id,
            transport,
            delegatee_key,
            dht,
            measure,
            transaction_replay,
            settings,
            observer,
            clock,
            rng,
        } = parts;
        let lifecycle_bounds = self::retention::lifecycle_bounds(dht.successors().capacity());
        Self {
            network_id,
            transport,
            delegatee_rotation_lock: Mutex::new(()),
            delegatee_key: RwLock::new(delegatee_key),
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
            storage_repair_cooldowns: Mutex::new(StorageRepairCooldowns::default()),
            outbound_schedulers: OutboundSchedulers::new_with_clock(measure.clone(), clock.clone()),
            admission_watcher: payload_send::AdmissionWatcher::default(),
            measured_disconnects: Mutex::new(BTreeMap::new()),
            candidate_dial_backoff: Mutex::new(BTreeMap::new()),
            measure,
            transaction_replay,
            observer,
            clock,
            rng,
        }
    }

    /// Return the epoch [`SharedClock`] capability bound to this transport.
    pub fn clock(&self) -> &SharedClock {
        &self.clock
    }

    /// Return the injected trait-based [`SharedTransport`] backing this swarm transport.
    pub fn transport(&self) -> &SharedTransport {
        &self.transport
    }

    /// Read the current Unix epoch timestamp in milliseconds from this transport's [`SharedClock`].
    pub fn now_ms(&self) -> u128 {
        self.clock.now_ms()
    }

    /// Read the current Unix epoch timestamp in milliseconds as `i64` from this transport's
    /// [`SharedClock`].
    pub(crate) fn now_ms_i64(&self) -> i64 {
        self.clock.now_ms_i64()
    }

    /// Return the [`SharedRng`] capability bound to this transport.
    pub fn rng(&self) -> &SharedRng {
        &self.rng
    }

    /// Record that an unadmitted candidate dial attempt to `peer` failed or timed out before
    /// admission, applying bounded exponential backoff to automatic topology-report dials.
    pub(crate) fn record_candidate_dial_failure(&self, peer: Did) {
        if peer == self.dht.did {
            return;
        }
        let now_ms = self.now_ms_i64();
        let Ok(mut map) = self.candidate_dial_backoff.lock() else {
            return;
        };
        let failures = map
            .get(&peer)
            .map_or(1, |entry| entry.failures.saturating_add(1));
        let shift = failures.saturating_sub(1).min(4);
        let delay_ms = CANDIDATE_DIAL_BACKOFF_BASE_MS
            .saturating_mul(1i64 << shift)
            .min(CANDIDATE_DIAL_BACKOFF_MAX_MS);
        if !map.contains_key(&peer) && map.len() >= CANDIDATE_DIAL_BACKOFF_CAPACITY {
            if let Some(evict_peer) = map
                .iter()
                .min_by_key(|(_, entry)| entry.retry_after_ms)
                .map(|(&candidate, _)| candidate)
            {
                map.remove(&evict_peer);
            }
        }
        map.insert(peer, CandidateDialBackoff {
            failures,
            retry_after_ms: now_ms.saturating_add(delay_ms),
        });
    }

    /// Clear any candidate dial backoff recorded for `peer`.
    pub(crate) fn clear_candidate_dial_backoff(&self, peer: Did) {
        if let Ok(mut map) = self.candidate_dial_backoff.lock() {
            map.remove(&peer);
        }
    }

    /// Return whether automatic topology-report dials to `peer` are currently in backoff.
    pub(crate) fn is_candidate_dial_backed_off(&self, peer: Did) -> bool {
        let now_ms = self.now_ms_i64();
        let Ok(map) = self.candidate_dial_backoff.lock() else {
            return false;
        };
        map.get(&peer)
            .is_some_and(|entry| now_ms < entry.retry_after_ms)
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

    /// Whether steady-state [`StorageSyncPurpose::AdditiveRepair`] for `cursor` is still inside
    /// its unchanged-topology rebroadcast cooldown at `(topology_epoch, now_ms)`.
    ///
    /// [`StorageSyncPurpose::OwnershipHandoff`] is never throttled, and advancing
    /// `topology_epoch` clears all cooldown entries immediately.
    pub(crate) fn is_storage_repair_cooled_down(
        &self,
        cursor: &StorageSyncDeliveryCursor,
        topology_epoch: u64,
        now_ms: i64,
    ) -> Result<bool> {
        if cursor.purpose() != StorageSyncPurpose::AdditiveRepair {
            return Ok(false);
        }
        let mut cooldowns = self
            .storage_repair_cooldowns
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        if cooldowns.topology_epoch != topology_epoch {
            cooldowns.sent.clear();
            cooldowns.topology_epoch = topology_epoch;
            return Ok(false);
        }
        Ok(cooldowns.sent.get(cursor).is_some_and(|&sent_at| {
            now_ms >= sent_at
                && now_ms.saturating_sub(sent_at) < STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS
        }))
    }

    /// Record that steady-state [`StorageSyncPurpose::AdditiveRepair`] for `cursor` was sent at
    /// `(topology_epoch, now_ms)`, pruning expired entries and bounding table size.
    pub(crate) fn record_storage_repair_sent(
        &self,
        cursor: StorageSyncDeliveryCursor,
        topology_epoch: u64,
        now_ms: i64,
    ) -> Result<()> {
        if cursor.purpose() != StorageSyncPurpose::AdditiveRepair {
            return Ok(());
        }
        let mut cooldowns = self
            .storage_repair_cooldowns
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        if cooldowns.topology_epoch != topology_epoch {
            cooldowns.sent.clear();
            cooldowns.topology_epoch = topology_epoch;
        }
        cooldowns.sent.retain(|_, &mut sent_at| {
            now_ms >= sent_at
                && now_ms.saturating_sub(sent_at) < STORAGE_REPAIR_REBROADCAST_COOLDOWN_MS
        });
        if cooldowns.sent.len() >= STORAGE_REPAIR_COOLDOWN_MAX_ENTRIES
            && !cooldowns.sent.contains_key(&cursor)
        {
            if let Some(oldest) = cooldowns
                .sent
                .iter()
                .min_by_key(|(_, &sent_at)| sent_at)
                .map(|(key, _)| key.clone())
            {
                cooldowns.sent.remove(&oldest);
            }
        }
        cooldowns.sent.insert(cursor, now_ms);
        Ok(())
    }

    /// Return the current entry count in the repair cooldown table for unit tests.
    #[cfg(test)]
    pub(crate) fn storage_repair_cooldown_len_for_test(&self) -> Result<usize> {
        let cooldowns = self
            .storage_repair_cooldowns
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(cooldowns.sent.len())
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn storage_repair_cursor_for_simulation(&self) -> Result<Option<String>> {
        let cursor = self
            .storage_repair_cursor
            .lock()
            .map_err(|_| Error::LockPoisoned)?;
        Ok(cursor.as_ref().map(|cursor| format!("{cursor:?}")))
    }

    /// The session key this node currently signs with.
    pub(crate) fn delegatee_key(&self) -> DelegateeKey {
        self.delegatee_key
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Validate `delegatee_key` without mutating state: verifies DID ownership, delegator
    /// authorization signature, and strict signing-horizon advancement relative to the current key.
    pub(crate) fn validate_next_delegatee_key(&self, delegatee_key: &DelegateeKey) -> Result<()> {
        if delegatee_key.delegator_did() != self.dht.did {
            return Err(Error::VerifySignatureFailed);
        }
        let delegation = delegatee_key.delegation();
        delegation.verify_delegator_authorization_at(delegation.created_at_ms())?;
        let next_horizon = crate::swarm::signing_horizon_ms(delegation.expires_at_ms());
        let current = self
            .delegatee_key
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let current_horizon =
            crate::swarm::signing_horizon_ms(current.delegation().expires_at_ms());
        if next_horizon <= current_horizon {
            return Err(Error::DelegationExpiresWithinMargin);
        }
        Ok(())
    }

    /// Infallibly install an already-validated `delegatee_key`.
    pub(crate) fn commit_delegatee_key_unchecked(&self, delegatee_key: DelegateeKey) {
        *self
            .delegatee_key
            .write()
            .unwrap_or_else(PoisonError::into_inner) = delegatee_key;
    }

    /// Acquire exclusive session-key rotation right, recovering across poisoned mutex state so
    /// two-phase preparation and commit remain infallible and serialized against
    /// [`Self::set_delegatee_key`].
    pub(crate) fn lock_delegatee_rotation(&self) -> MutexGuard<'_, ()> {
        self.delegatee_rotation_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the session key this node signs with, when it carries a valid delegator
    /// signature for the same account DID and strictly advances the signing horizon.
    pub(crate) fn set_delegatee_key(&self, delegatee_key: DelegateeKey) -> Result<()> {
        let _rotation_guard = self.lock_delegatee_rotation();
        if delegatee_key.delegator_did() != self.dht.did {
            return Err(Error::VerifySignatureFailed);
        }
        let delegation = delegatee_key.delegation();
        delegation.verify_delegator_authorization_at(delegation.created_at_ms())?;
        let next_horizon = crate::swarm::signing_horizon_ms(delegation.expires_at_ms());
        let mut current = self
            .delegatee_key
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let current_horizon =
            crate::swarm::signing_horizon_ms(current.delegation().expires_at_ms());
        if next_horizon <= current_horizon {
            return Err(Error::DelegationExpiresWithinMargin);
        }
        *current = delegatee_key;
        Ok(())
    }

    /// The session this node authenticates with: its public half and account authorization.
    pub(crate) fn delegation(&self) -> Delegation {
        self.delegatee_key().delegation()
    }

    /// Storage virtual-node positions required by this DHT protocol mode.
    pub(crate) fn dht_virtual_nodes(&self) -> u16 {
        self.dht_virtual_nodes
    }

    /// Return the DHT protocol mode advertised and required by this transport.
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

    /// Return a shared handle to the node-wide inbound capacity ledger.
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
        let now_ms = self.now_ms_i64();
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

    /// Clear any recorded disconnect timestamp for `attempt` if it is still the active slot.
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
        rings_transport::connections::dummy_generation_id(&self.connection).map_err(Into::into)
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

    /// Close the underlying physical transport connection.
    async fn close(&self) -> Result<()> {
        self.connection.close().await.map_err(Into::into)
    }

    /// Return the current physical transport connection state.
    pub fn connection_state(&self) -> TransportConnectionState {
        self.connection.connection_state()
    }

    #[cfg(all(
        test,
        not(all(feature = "wasm", target_family = "wasm")),
        not(feature = "dummy")
    ))]
    pub(crate) fn physical_close_witness(
        &self,
    ) -> Result<rings_transport::connections::NativePhysicalCloseWitness> {
        rings_transport::connections::physical_close_witness(&self.connection).map_err(Into::into)
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
