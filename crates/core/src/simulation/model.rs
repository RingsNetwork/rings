//! Pure state-transition model and stable trace format for sync-storm tests.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;

/// Stable node identifier used only in simulation traces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct SimNodeId(pub(crate) u16);

/// Causal event identifier, distinct from every clock domain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct SimEventId(pub(crate) u64);

/// Stable semantic identity for one real dummy-transport frame.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimFrameIdentity {
    /// Local Rings DID receiving the dummy callback.
    pub(crate) local_did: String,
    /// Remote Rings DID associated with the connection.
    pub(crate) peer_did: String,
    /// Stable dummy connection generation identifier.
    pub(crate) connection_generation: String,
    /// Transaction identifier decoded through the production wire format.
    pub(crate) transaction_id: Option<uuid::Uuid>,
}

/// Message class observed at the real dummy-transport boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) enum SimTransferClass {
    /// Chord and liveness control traffic.
    Control,
    /// Storage synchronization traffic.
    Storage,
    /// Chunk frames waiting for reassembly.
    Reassembly,
    /// End-to-end encrypted traffic.
    E2e,
    /// Application traffic unrelated to this scenario.
    Application,
}

impl SimTransferClass {
    const fn index(self) -> usize {
        match self {
            Self::Control => 0,
            Self::Storage => 1,
            Self::Reassembly => 2,
            Self::E2e => 3,
            Self::Application => 4,
        }
    }
}

/// Lifecycle state of one stable connection generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum SimConnectionState {
    /// The production generation is admitted and ready.
    Active,
    /// Liveness removed the generation before physical teardown completed.
    Removed,
    /// The production generation has completed teardown.
    Closed,
}

/// Typed result of a production maintenance pass.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum SimMaintenanceOutcome {
    /// No repair request was pending.
    Idle,
    /// The bounded repair pass completed.
    Complete,
    /// Repair remains requested for a later bounded pass.
    Deferred,
}

/// One deterministic action consumed by [`SimState::transition`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum SimAction {
    /// Inject the initial storage synchronization workload.
    InjectSync {
        /// Node anchoring the scenario root.
        node: SimNodeId,
        /// Number of non-repair entries submitted.
        entries: u64,
    },
    /// Advance virtual monotonic time using the roadmap's stable action name.
    AdvanceVirtualTime {
        /// Milliseconds advanced on the virtual clock.
        delta_ms: u64,
    },
    /// Observe a production frame before controlled-queue admission.
    SubmitFrame {
        /// Stable transfer identifier.
        transfer_id: u64,
        /// Node submitting the frame.
        node: SimNodeId,
        /// Semantic transfer class.
        class: SimTransferClass,
        /// Stable production identity.
        identity: SimFrameIdentity,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Admit a submitted production frame to the controlled queue.
    AdmitFrame {
        /// Stable transfer identifier from the harness.
        transfer_id: u64,
        /// Node whose resources account for this transfer.
        node: SimNodeId,
        /// Semantic transfer class.
        class: SimTransferClass,
        /// Accounted bytes.
        bytes: u64,
        /// Virtual monotonic queue-admission time.
        enqueued_virtual_ms: u64,
        /// Class-specific virtual deadline.
        deadline_virtual_ms: Option<u64>,
        /// Stable production identity.
        identity: SimFrameIdentity,
        /// Explicit causal parent, normally [`SimAction::SubmitFrame`].
        causal_parent: Option<SimEventId>,
    },
    /// Select an admitted frame for real dummy delivery.
    DispatchFrame {
        /// Stable admitted transfer identifier.
        transfer_id: u64,
        /// Explicit causal parent, normally [`SimAction::AdmitFrame`].
        causal_parent: Option<SimEventId>,
    },
    /// Complete delivery of a previously dispatched frame.
    DeliverFrame {
        /// Stable admitted transfer identifier.
        transfer_id: u64,
        /// Explicit causal parent, normally [`SimAction::DispatchFrame`].
        causal_parent: Option<SimEventId>,
    },
    /// Advance one real chunk through production reassembly.
    AdvanceReassembly {
        /// Node receiving the chunk.
        node: SimNodeId,
        /// Stable chunk-frame transfer identifier.
        transfer_id: u64,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Observe a production reassembly handoff barrier verdict for control.
    ObserveReassemblyBarrier {
        /// Node whose inbound actor owns the barrier.
        node: SimNodeId,
        /// Exact dispatched control frame checked by the barrier.
        control_transfer_id: u64,
        /// Stable connection generation carrying the control event.
        generation: String,
        /// Exact control transaction checked by the barrier.
        transaction_id: Option<uuid::Uuid>,
        /// Whether the production barrier blocked the control event.
        blocked_control: bool,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Observe one durable entry effect at the production handler boundary.
    PersistOneEntry {
        /// Node persisting the entry.
        node: SimNodeId,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Observe the storage actor yielding between two entry effects.
    YieldActor {
        /// Node whose actor yielded.
        node: SimNodeId,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Record one stable production connection-generation lifecycle event.
    ConnectionLifecycle {
        /// Local node owning the connection.
        node: SimNodeId,
        /// Remote peer node.
        peer: SimNodeId,
        /// Stable local Rings DID for this endpoint.
        local_did: String,
        /// Stable remote Rings DID for this endpoint.
        peer_did: String,
        /// Stable dummy connection generation.
        generation: String,
        /// Observed lifecycle state.
        state: SimConnectionState,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Record a real Stabilizer liveness pass and verdict.
    RunLiveness {
        /// Node executing liveness maintenance.
        node: SimNodeId,
        /// Peer checked by the pass.
        peer: SimNodeId,
        /// Stable generation checked by the verdict.
        generation: String,
        /// Probe transaction identity when a probe was emitted.
        transaction_id: Option<uuid::Uuid>,
        /// Whether the production verdict removed the generation.
        removed: bool,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Record a bounded production repair-maintenance pass.
    RunMaintenance {
        /// Node executing maintenance.
        node: SimNodeId,
        /// Typed pass result.
        outcome: SimMaintenanceOutcome,
        /// Stable repair-window cursor after the pass.
        repair_cursor: Option<String>,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Mark the point after which no new workload is injected.
    StopStorm {
        /// Node anchoring the global stop event.
        node: SimNodeId,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Inject an actual peer failure; disconnects are valid only after this.
    InjectPeerFailure {
        /// Observer that may later declare the peer unavailable.
        node: SimNodeId,
        /// Failed peer.
        peer: SimNodeId,
    },
    /// Observe a production liveness decision that removes a peer.
    Disconnect {
        /// Node making the liveness decision.
        node: SimNodeId,
        /// Peer removed by that decision.
        peer: SimNodeId,
        /// Explicit causal parent.
        causal_parent: Option<SimEventId>,
    },
    /// Observe repair work produced after a topology/liveness transition.
    ScheduleRepair {
        /// Node scheduling the repair.
        node: SimNodeId,
        /// Number of entries added to repair synchronization.
        entries: u64,
        /// Explicit causal parent, normally the disconnect event.
        causal_parent: Option<SimEventId>,
    },
}

impl SimAction {
    fn causal_parent(&self) -> Option<SimEventId> {
        match self {
            Self::SubmitFrame { causal_parent, .. }
            | Self::AdmitFrame { causal_parent, .. }
            | Self::DispatchFrame { causal_parent, .. }
            | Self::DeliverFrame { causal_parent, .. }
            | Self::AdvanceReassembly { causal_parent, .. }
            | Self::ObserveReassemblyBarrier { causal_parent, .. }
            | Self::PersistOneEntry { causal_parent, .. }
            | Self::YieldActor { causal_parent, .. }
            | Self::ConnectionLifecycle { causal_parent, .. }
            | Self::RunLiveness { causal_parent, .. }
            | Self::RunMaintenance { causal_parent, .. }
            | Self::StopStorm { causal_parent, .. }
            | Self::Disconnect { causal_parent, .. }
            | Self::ScheduleRepair { causal_parent, .. } => *causal_parent,
            Self::InjectSync { .. }
            | Self::AdvanceVirtualTime { .. }
            | Self::InjectPeerFailure { .. } => None,
        }
    }

    const fn explicit_node(&self) -> Option<SimNodeId> {
        match self {
            Self::InjectSync { node, .. }
            | Self::SubmitFrame { node, .. }
            | Self::AdmitFrame { node, .. }
            | Self::AdvanceReassembly { node, .. }
            | Self::ObserveReassemblyBarrier { node, .. }
            | Self::PersistOneEntry { node, .. }
            | Self::YieldActor { node, .. }
            | Self::ConnectionLifecycle { node, .. }
            | Self::RunLiveness { node, .. }
            | Self::RunMaintenance { node, .. }
            | Self::StopStorm { node, .. }
            | Self::InjectPeerFailure { node, .. }
            | Self::Disconnect { node, .. }
            | Self::ScheduleRepair { node, .. } => Some(*node),
            Self::AdvanceVirtualTime { .. }
            | Self::DispatchFrame { .. }
            | Self::DeliverFrame { .. } => None,
        }
    }
}

/// Observable output of one pure transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimOutput {
    /// Event created by the transition.
    pub(crate) event_id: SimEventId,
    /// Control-frame queue latency, populated only for a control delivery.
    pub(crate) control_latency_ms: Option<u64>,
}

/// Structural failures that make a trace invalid rather than unsafe.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum SimTransitionError {
    /// Virtual time exceeded its representation.
    #[error("virtual time overflowed")]
    TimeOverflow,
    /// An arithmetic counter exceeded its representation.
    #[error("simulation counter overflowed")]
    CounterOverflow,
    /// A transfer identifier was admitted more than once.
    #[error("transfer {transfer_id} was admitted more than once")]
    DuplicateTransfer {
        /// Duplicated identifier.
        transfer_id: u64,
    },
    /// A delivery did not refer to an admitted transfer.
    #[error("transfer {transfer_id} was delivered without admission")]
    UnknownTransfer {
        /// Unknown identifier.
        transfer_id: u64,
    },
    /// A transfer action did not follow the required production phase.
    #[error("transfer {transfer_id} expected phase {expected}, observed {observed}")]
    InvalidTransferPhase {
        /// Stable transfer identifier.
        transfer_id: u64,
        /// Required preceding phase.
        expected: &'static str,
        /// Observed incompatible phase.
        observed: &'static str,
    },
    /// A frame changed identity between production boundaries.
    #[error("transfer {transfer_id} changed identity between submission and admission")]
    FrameIdentityMismatch {
        /// Stable transfer identifier.
        transfer_id: u64,
    },
    /// An action named a missing, closed, or mismatched connection generation.
    #[error("generation {generation} is not active for {node:?}->{peer:?}")]
    InactiveGeneration {
        /// Local node.
        node: SimNodeId,
        /// Remote peer.
        peer: SimNodeId,
        /// Stable production generation.
        generation: String,
    },
    /// A lifecycle transition contradicted the current generation state.
    #[error("invalid lifecycle transition for {node:?}->{peer:?} generation {generation}")]
    InvalidLifecycle {
        /// Local node.
        node: SimNodeId,
        /// Remote peer.
        peer: SimNodeId,
        /// Stable production generation.
        generation: String,
    },
    /// A liveness verdict lacked matching probe/generation evidence.
    #[error("invalid liveness verdict for {node:?}->{peer:?} generation {generation}")]
    InvalidLivenessVerdict {
        /// Local node.
        node: SimNodeId,
        /// Remote peer.
        peer: SimNodeId,
        /// Stable production generation.
        generation: String,
    },
    /// Repair was scheduled without a preceding removal intent.
    #[error("node {node:?} scheduled repair without a removal intent")]
    MissingRepairIntent {
        /// Node scheduling repair.
        node: SimNodeId,
    },
    /// A yield was not causally attached to a durable entry effect.
    #[error("node {node:?} yielded without an unyielded persistence effect")]
    YieldWithoutPersistence {
        /// Node whose actor yielded.
        node: SimNodeId,
    },
    /// New workload or frames appeared after the explicit stop boundary.
    #[error("action {action} occurred after StopStorm")]
    ActionAfterStop {
        /// Stable action name.
        action: &'static str,
    },
    /// An action referred to a causal event that does not exist yet.
    #[error("causal parent {parent:?} does not precede event {next:?}")]
    InvalidCausalParent {
        /// Invalid parent.
        parent: SimEventId,
        /// Event that would have been allocated.
        next: SimEventId,
    },
    /// An action did not name the event produced by its required predecessor.
    #[error("action expected causal parent {expected:?}, observed {observed:?}")]
    UnexpectedCausalParent {
        /// Required predecessor event.
        expected: SimEventId,
        /// Supplied predecessor event.
        observed: Option<SimEventId>,
    },
}

/// Safety and liveness limits checked independently of trace construction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimLimits {
    /// Maximum admitted bytes owned by one node.
    pub(crate) node_bytes: u64,
    /// Maximum admitted bytes across the scenario.
    pub(crate) global_bytes: u64,
    /// Maximum acceptable control-frame queue latency.
    pub(crate) control_deadline_ms: u64,
    /// Maximum repair amplification relative to the initial workload.
    pub(crate) repair_amplification: u64,
}

/// Named safety or liveness proposition violated by a completed observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum SimInvariantViolation {
    /// A node exceeded its configured byte bound.
    NodeCapacity {
        /// Node exceeding the limit.
        node: SimNodeId,
        /// Maximum bytes observed.
        observed: u64,
        /// Configured limit.
        limit: u64,
    },
    /// Aggregate bytes exceeded the configured bound.
    GlobalCapacity {
        /// Maximum bytes observed.
        observed: u64,
        /// Configured limit.
        limit: u64,
    },
    /// A control frame missed its virtual-time deadline.
    ControlStarvation {
        /// Maximum control latency observed.
        observed_ms: u64,
        /// Configured deadline.
        deadline_ms: u64,
    },
    /// Liveness removed a peer that had no injected failure.
    FalseDisconnect {
        /// Observer making the incorrect decision.
        node: SimNodeId,
        /// Healthy peer that was removed.
        peer: SimNodeId,
    },
    /// Repair synchronization fed back from a false disconnect or amplified
    /// beyond the allowed multiple.
    RepairStorm {
        /// Repair entries scheduled.
        repair_entries: u64,
        /// Initial non-repair entries.
        initial_entries: u64,
        /// Allowed amplification multiple.
        allowed_amplification: u64,
    },
    /// Initial storage work never made durable progress.
    NoStorageProgress,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SubmittedTransfer {
    node: SimNodeId,
    class: SimTransferClass,
    identity: SimFrameIdentity,
    event_id: SimEventId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingTransfer {
    node: SimNodeId,
    class: SimTransferClass,
    bytes: u64,
    enqueued_at_ms: u64,
    deadline_virtual_ms: Option<u64>,
    identity: SimFrameIdentity,
    admission_event: SimEventId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DispatchedTransfer {
    pending: PendingTransfer,
    dispatch_event: SimEventId,
    reassembly_event: Option<SimEventId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct FrameTransactionEvidence {
    submitted: u64,
    delivered: u64,
    delivered_event: Option<SimEventId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SimConnectionGeneration {
    generation: String,
    local_did: String,
    peer_did: String,
    state: SimConnectionState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
enum SimPeerHealth {
    Healthy,
    Failed,
    Removed,
}

/// Stable event record with explicit virtual time and causal parent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimTraceEvent {
    /// Monotonic causal sequence allocated by the model.
    pub(crate) id: SimEventId,
    /// Sequence local to the acting node, independent of every clock domain.
    pub(crate) node_sequence: Option<u64>,
    /// Virtual monotonic time at which the action was observed.
    pub(crate) virtual_ms: u64,
    /// Explicit parent in the event partial order.
    pub(crate) causal_parent: Option<SimEventId>,
    /// Action applied at this boundary.
    pub(crate) action: SimAction,
    /// Transition output.
    pub(crate) output: SimOutput,
}

/// Ordered, serialization-stable trace used for deterministic replay.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimTrace {
    events: Vec<SimTraceEvent>,
}

impl SimTrace {
    /// Events in deterministic causal-sequence order.
    pub(crate) fn events(&self) -> &[SimTraceEvent] {
        &self.events
    }

    /// SHA-256 of the canonical JSON representation used by CI comparisons.
    pub(crate) fn digest(&self) -> Result<[u8; 32], serde_json::Error> {
        let bytes = self.canonical_json()?;
        Ok(sha2::Sha256::digest(bytes).into())
    }

    /// Canonical byte representation persisted for replay diagnostics.
    pub(crate) fn canonical_json(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }
}

/// Pure state accumulated from observations of the production protocol path.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimState {
    virtual_ms: u64,
    next_event: u64,
    next_node_sequence: BTreeMap<SimNodeId, u64>,
    submitted: BTreeMap<u64, SubmittedTransfer>,
    pending: BTreeMap<u64, PendingTransfer>,
    in_flight: BTreeMap<u64, DispatchedTransfer>,
    outbound_queues: BTreeMap<(SimNodeId, String, SimTransferClass), BTreeSet<u64>>,
    inbound_lanes: BTreeMap<(SimNodeId, SimTransferClass), BTreeSet<u64>>,
    current_node_bytes: BTreeMap<SimNodeId, u64>,
    current_class_bytes: BTreeMap<(SimNodeId, SimTransferClass), u64>,
    peak_node_bytes: BTreeMap<SimNodeId, u64>,
    current_global_bytes: u64,
    peak_global_bytes: u64,
    initial_entries: u64,
    persisted_entries: u64,
    repair_entries: u64,
    max_control_latency_ms: u64,
    max_missed_control_latency_ms: Option<(u64, u64)>,
    injected_failures: BTreeSet<(SimNodeId, SimNodeId)>,
    disconnects: BTreeSet<(SimNodeId, SimNodeId)>,
    connection_generations: BTreeMap<(SimNodeId, SimNodeId), SimConnectionGeneration>,
    peer_health: BTreeMap<(SimNodeId, SimNodeId), SimPeerHealth>,
    liveness_verdicts: BTreeMap<(SimNodeId, SimNodeId), (bool, SimEventId)>,
    repair_cursors: BTreeMap<SimNodeId, Option<String>>,
    repair_intents: BTreeMap<SimNodeId, SimEventId>,
    failure_events: BTreeMap<(SimNodeId, SimNodeId), SimEventId>,
    frame_transactions:
        BTreeMap<uuid::Uuid, BTreeMap<(String, SimTransferClass), FrameTransactionEvidence>>,
    unyielded_persistence: BTreeMap<SimNodeId, SimEventId>,
    reassembly_advances: u64,
    reassembly_barriers: u64,
    blocked_control_barriers: u64,
    barrier_events: BTreeMap<u64, (SimEventId, bool)>,
    actor_yields: u64,
    maintenance_runs: u64,
    storm_stopped: bool,
    class_deliveries: [u64; 5],
    trace: SimTrace,
}

/// Compact deterministic state snapshot printed with a failing replay.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SimSnapshot {
    /// Current monotonic virtual time.
    pub(crate) virtual_ms: u64,
    /// Frames still admitted but not delivered.
    pub(crate) pending_transfers: usize,
    /// Maximum retained bytes observed on any node.
    pub(crate) peak_node_bytes: u64,
    /// Maximum retained bytes observed across the scenario.
    pub(crate) peak_global_bytes: u64,
    /// Initial non-repair work.
    pub(crate) initial_entries: u64,
    /// Entries observed durably persisted.
    pub(crate) persisted_entries: u64,
    /// Repair work scheduled after liveness changes.
    pub(crate) repair_entries: u64,
    /// Largest delivered control-frame latency.
    pub(crate) max_control_latency_ms: u64,
    /// Number of connection generations removed by liveness.
    pub(crate) disconnects: usize,
    /// Number of stable connection generations observed.
    pub(crate) connection_generations: usize,
    /// Number of production liveness verdicts observed.
    pub(crate) liveness_verdicts: usize,
    /// Number of healthy generations removed by those verdicts.
    pub(crate) liveness_removals: usize,
    /// Number of real reassembly advances observed.
    pub(crate) reassembly_advances: u64,
    /// Number of real reassembly handoff barrier verdicts observed.
    pub(crate) reassembly_barriers: u64,
    /// Number of those barriers that rejected control.
    pub(crate) blocked_control_barriers: u64,
    /// Number of actor-yield witnesses observed.
    pub(crate) actor_yields: u64,
    /// Number of bounded maintenance passes observed.
    pub(crate) maintenance_runs: u64,
    /// Whether workload injection was explicitly stopped.
    pub(crate) storm_stopped: bool,
    /// Delivery counts in stable transfer-class order.
    pub(crate) class_deliveries: [u64; 5],
}

impl SimState {
    /// Return every violated proposition in deterministic order.
    pub(crate) fn invariant_violations(&self, limits: SimLimits) -> Vec<SimInvariantViolation> {
        let mut violations = Vec::new();
        for (&node, &observed) in &self.peak_node_bytes {
            if observed > limits.node_bytes {
                violations.push(SimInvariantViolation::NodeCapacity {
                    node,
                    observed,
                    limit: limits.node_bytes,
                });
            }
        }
        if self.peak_global_bytes > limits.global_bytes {
            violations.push(SimInvariantViolation::GlobalCapacity {
                observed: self.peak_global_bytes,
                limit: limits.global_bytes,
            });
        }
        let pending_control_latency = self
            .pending
            .values()
            .chain(self.in_flight.values().map(|transfer| &transfer.pending))
            .filter(|pending| pending.class == SimTransferClass::Control)
            .map(|pending| self.virtual_ms.saturating_sub(pending.enqueued_at_ms))
            .max()
            .unwrap_or(0);
        let control_latency = self.max_control_latency_ms.max(pending_control_latency);
        let pending_explicit_miss = self
            .pending
            .values()
            .chain(self.in_flight.values().map(|transfer| &transfer.pending))
            .filter(|pending| pending.class == SimTransferClass::Control)
            .filter_map(|pending| {
                let deadline = pending.deadline_virtual_ms?;
                (self.virtual_ms > deadline).then(|| {
                    (
                        self.virtual_ms.saturating_sub(pending.enqueued_at_ms),
                        deadline.saturating_sub(pending.enqueued_at_ms),
                    )
                })
            })
            .max();
        let explicit_miss = self
            .max_missed_control_latency_ms
            .max(pending_explicit_miss);
        if let Some((observed_ms, deadline_ms)) = explicit_miss {
            violations.push(SimInvariantViolation::ControlStarvation {
                observed_ms,
                deadline_ms,
            });
        } else if control_latency > limits.control_deadline_ms {
            violations.push(SimInvariantViolation::ControlStarvation {
                observed_ms: control_latency,
                deadline_ms: limits.control_deadline_ms,
            });
        }
        for &(node, peer) in &self.disconnects {
            if !self.injected_failures.contains(&(node, peer)) {
                violations.push(SimInvariantViolation::FalseDisconnect { node, peer });
            }
        }
        let allowed_repairs = self
            .initial_entries
            .saturating_mul(limits.repair_amplification);
        let has_false_disconnect = self
            .disconnects
            .iter()
            .any(|edge| !self.injected_failures.contains(edge));
        if self.repair_entries > allowed_repairs
            || (has_false_disconnect && self.repair_entries > 0)
        {
            violations.push(SimInvariantViolation::RepairStorm {
                repair_entries: self.repair_entries,
                initial_entries: self.initial_entries,
                allowed_amplification: limits.repair_amplification,
            });
        }
        if self.initial_entries > 0 && self.persisted_entries == 0 {
            violations.push(SimInvariantViolation::NoStorageProgress);
        }
        violations
    }

    /// Stable trace accumulated by this state.
    pub(crate) const fn trace(&self) -> &SimTrace {
        &self.trace
    }

    /// Compact stable state needed to diagnose and replay a failed scenario.
    pub(crate) fn snapshot(&self) -> SimSnapshot {
        SimSnapshot {
            virtual_ms: self.virtual_ms,
            pending_transfers: self.pending.len().saturating_add(self.in_flight.len()),
            peak_node_bytes: self.peak_node_bytes.values().copied().max().unwrap_or(0),
            peak_global_bytes: self.peak_global_bytes,
            initial_entries: self.initial_entries,
            persisted_entries: self.persisted_entries,
            repair_entries: self.repair_entries,
            max_control_latency_ms: self.max_control_latency_ms,
            disconnects: self.disconnects.len(),
            connection_generations: self.connection_generations.len(),
            liveness_verdicts: self.liveness_verdicts.len(),
            liveness_removals: self
                .liveness_verdicts
                .values()
                .filter(|(removed, _)| *removed)
                .count(),
            reassembly_advances: self.reassembly_advances,
            reassembly_barriers: self.reassembly_barriers,
            blocked_control_barriers: self.blocked_control_barriers,
            actor_yields: self.actor_yields,
            maintenance_runs: self.maintenance_runs,
            storm_stopped: self.storm_stopped,
            class_deliveries: self.class_deliveries,
        }
    }

    /// Number of deliveries by semantic transfer class.
    pub(crate) const fn class_deliveries(&self, class: SimTransferClass) -> u64 {
        self.class_deliveries[class.index()]
    }
}

mod transition;

#[cfg(test)]
mod tests;
