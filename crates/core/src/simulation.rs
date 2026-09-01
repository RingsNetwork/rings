//! Deterministic effect boundary for native dummy-transport simulations.
//!
//! The boundary is intentionally thread-local. Native dummy tests use Tokio's
//! current-thread runtime, and the controlled dummy transport is thread-local
//! for the same reason. Production and browser builds cannot reach this module.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use rings_transport::connections::dummy_controlled;
use tokio::sync::Notify;

use crate::swarm::transport::PEER_LIVENESS_IDLE_MS;

mod delivery;
pub(crate) mod model;
mod observations;
mod service;
mod spawn;

use delivery::inspect_message;
use delivery::refresh_delivery_cache;
use delivery::remove_cached_delivery;
pub(crate) use delivery::DeliveryStrategy;
pub(crate) use delivery::ScheduledDelivery;
pub(crate) use delivery::ScheduledDeliveryClass;
pub(crate) use observations::observe_inbound_capacity;
pub(crate) use observations::observe_outbound_global_capacity;
pub(crate) use observations::observe_outbound_peer_capacity;
pub(crate) use observations::observe_reassembly_capacity;
pub(crate) use observations::record_barrier_control_blocked;
pub(crate) use observations::record_barrier_control_deadline_miss;
pub(crate) use observations::record_outbound_submission;
pub(crate) use observations::record_protection_violation;
pub(crate) use observations::record_reassembly_advance;
pub(crate) use observations::record_repair_entries;
pub(crate) use observations::record_storage_actor_yield;
pub(crate) use observations::record_storage_persisted;
pub(crate) use observations::record_storage_progress;
pub(crate) use observations::record_storage_progress_between_entries;
pub(crate) use observations::signal_storage_progress_probe;
pub(crate) use observations::storage_progress_epoch;
use observations::DeadlineMissWitness;
pub(crate) use observations::ProductionCapacityObservations;
pub(crate) use observations::ProductionTraceObservation;
pub(crate) use service::wait_reassembly_service;
pub(crate) use spawn::spawn_storage_progress_observer;

const UUID_VERSION_4: u8 = 0x40;
const UUID_RFC4122_VARIANT: u8 = 0x80;
pub(crate) const CONTROL_DEADLINE_MS: u64 = PEER_LIVENESS_IDLE_MS as u64;

thread_local! {
    static RUNTIME: RefCell<Option<SimulationRuntimeState>> = const { RefCell::new(None) };
}

static SIMULATION_LOCK: Mutex<()> = Mutex::new(());

/// Scheduler and admission protections whose contribution is tested by #686.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProtectionProfile {
    class_reservations: bool,
    bounded_control_burst: bool,
    barrier_control_exemption: bool,
    per_entry_yield: bool,
}

impl ProtectionProfile {
    /// Every production protection enabled.
    pub(crate) const ALL_ENABLED: Self = Self {
        class_reservations: true,
        bounded_control_burst: true,
        barrier_control_exemption: true,
        per_entry_yield: true,
    };

    /// Every protection disabled, reproducing the pre-#683 behavior.
    pub(crate) const LEGACY_ALL_DISABLED: Self = Self {
        class_reservations: false,
        bounded_control_burst: false,
        barrier_control_exemption: false,
        per_entry_yield: false,
    };

    /// Return a profile with only class reservations disabled.
    pub(crate) const fn without_class_reservations() -> Self {
        Self {
            class_reservations: false,
            ..Self::ALL_ENABLED
        }
    }

    /// Return a profile with only the bounded control burst disabled.
    pub(crate) const fn without_bounded_control_burst() -> Self {
        Self {
            bounded_control_burst: false,
            ..Self::ALL_ENABLED
        }
    }

    /// Return a profile with only the barrier control exemption disabled.
    pub(crate) const fn without_barrier_control_exemption() -> Self {
        Self {
            barrier_control_exemption: false,
            ..Self::ALL_ENABLED
        }
    }

    /// Return a profile with only per-entry yielding disabled.
    pub(crate) const fn without_per_entry_yield() -> Self {
        Self {
            per_entry_yield: false,
            ..Self::ALL_ENABLED
        }
    }

    pub(crate) const fn class_reservations(self) -> bool {
        self.class_reservations
    }

    pub(crate) const fn bounded_control_burst(self) -> bool {
        self.bounded_control_burst
    }

    pub(crate) const fn barrier_control_exemption(self) -> bool {
        self.barrier_control_exemption
    }

    pub(crate) const fn per_entry_yield(self) -> bool {
        self.per_entry_yield
    }

    /// Stable name used by trace artifacts and replay commands.
    pub(crate) const fn name(self) -> &'static str {
        match (
            self.class_reservations,
            self.bounded_control_burst,
            self.barrier_control_exemption,
            self.per_entry_yield,
        ) {
            (true, true, true, true) => "all-enabled",
            (false, true, true, true) => "no-class-reservations",
            (true, false, true, true) => "no-bounded-control-burst",
            (true, true, false, true) => "no-barrier-control-exemption",
            (true, true, true, false) => "no-per-entry-yield",
            (false, false, false, false) => "legacy-all-disabled",
            _ => "custom",
        }
    }

    /// Layers disabled by this profile, in stable proposition order.
    pub(crate) fn disabled_layers(self) -> BTreeSet<ProtectionLayer> {
        let mut layers = BTreeSet::new();
        if !self.class_reservations {
            layers.insert(ProtectionLayer::ClassReservations);
        }
        if !self.bounded_control_burst {
            layers.insert(ProtectionLayer::BoundedControlBurst);
        }
        if !self.barrier_control_exemption {
            layers.insert(ProtectionLayer::BarrierControlExemption);
        }
        if !self.per_entry_yield {
            layers.insert(ProtectionLayer::PerEntryYield);
        }
        layers
    }
}

/// Production scheduling proposition exercised by a protection ablation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, serde::Serialize)]
pub(crate) enum ProtectionLayer {
    /// Non-control work may consume slots reserved for control admission.
    ClassReservations,
    /// Sustained control traffic may indefinitely preempt a runnable lower class.
    BoundedControlBurst,
    /// A control frame may wait behind a chunk-reassembly handoff.
    BarrierControlExemption,
    /// A storage batch may continue to its next entry without yielding the actor.
    PerEntryYield,
}

#[derive(Debug)]
struct SimulationRuntimeState {
    epoch_ms: u128,
    elapsed_ms: u128,
    uuid_seed: u64,
    next_uuid_sequence: u64,
    schedule_rng_state: u64,
    protection: ProtectionProfile,
    observations: ProtectionObservations,
    reassembly_service_enabled: bool,
    delivery_cache: BTreeMap<u64, ScheduledDelivery>,
    delivery_order: BTreeSet<u64>,
    non_control_delivery_order: BTreeSet<u64>,
    unreported_deliveries: BTreeSet<u64>,
    last_inspected_delivery: Option<u64>,
    outbound_submission_ms: BTreeMap<uuid::Uuid, u64>,
    capacity_observations: ProductionCapacityObservations,
    storage_progress_notify: Option<Arc<Notify>>,
    storage_progress_epoch: u64,
    production_trace_observations: Vec<ProductionTraceObservation>,
    reassembly_advances: BTreeMap<uuid::Uuid, u64>,
    repair_entries_observed: u64,
    artifact_identity: String,
    artifact_context: serde_json::Value,
}

impl SimulationRuntimeState {
    fn new(seed: u64, epoch_ms: u128, protection: ProtectionProfile) -> Self {
        Self {
            epoch_ms,
            elapsed_ms: 0,
            uuid_seed: dummy_controlled::mix_seed(seed),
            next_uuid_sequence: 0,
            schedule_rng_state: seed,
            protection,
            observations: ProtectionObservations::default(),
            reassembly_service_enabled: false,
            delivery_cache: BTreeMap::new(),
            delivery_order: BTreeSet::new(),
            non_control_delivery_order: BTreeSet::new(),
            unreported_deliveries: BTreeSet::new(),
            last_inspected_delivery: None,
            outbound_submission_ms: BTreeMap::new(),
            capacity_observations: ProductionCapacityObservations::default(),
            storage_progress_notify: None,
            storage_progress_epoch: 0,
            production_trace_observations: Vec::new(),
            reassembly_advances: BTreeMap::new(),
            repair_entries_observed: 0,
            artifact_identity: format!("seed-{seed}-{}", protection.name()),
            artifact_context: serde_json::json!({
                "seed": seed,
                "profile": protection.name(),
            }),
        }
    }

    fn advance(&mut self, duration: Duration) -> Result<u64, SimulationRuntimeError> {
        let delta_ms = duration.as_millis();
        let epoch_ms = self
            .epoch_ms
            .checked_add(delta_ms)
            .ok_or(SimulationRuntimeError::TimeOverflow)?;
        let elapsed_ms = self
            .elapsed_ms
            .checked_add(delta_ms)
            .ok_or(SimulationRuntimeError::TimeOverflow)?;
        let controlled_ms =
            u64::try_from(elapsed_ms).map_err(|_| SimulationRuntimeError::TimeOverflow)?;
        self.epoch_ms = epoch_ms;
        self.elapsed_ms = elapsed_ms;
        Ok(controlled_ms)
    }

    fn next_uuid(&mut self) -> uuid::Uuid {
        let sequence = self.next_uuid_sequence;
        self.next_uuid_sequence = self.next_uuid_sequence.wrapping_add(1);
        deterministic_uuid(self.uuid_seed, sequence)
    }

    fn choose_delivery(&mut self, pending: usize) -> Result<usize, SimulationRuntimeError> {
        let pending_u64 = u64::try_from(pending)
            .map_err(|_| SimulationRuntimeError::PendingDeliveryCountOverflow { pending })?;
        if pending_u64 == 0 {
            return Err(SimulationRuntimeError::EmptyDeliveryQueue);
        }
        self.schedule_rng_state = dummy_controlled::mix_seed(self.schedule_rng_state);
        let selected = self.schedule_rng_state % pending_u64;
        usize::try_from(selected)
            .map_err(|_| SimulationRuntimeError::PendingDeliveryCountOverflow { pending })
    }

    fn choose_delivery_sequence(&mut self) -> Result<u64, SimulationRuntimeError> {
        let Some(max_sequence) = self.delivery_order.last().copied() else {
            return Err(SimulationRuntimeError::EmptyDeliveryQueue);
        };
        self.schedule_rng_state = dummy_controlled::mix_seed(self.schedule_rng_state);
        let ticket = self.schedule_rng_state % max_sequence.saturating_add(1);
        self.delivery_order
            .range(ticket..)
            .next()
            .or_else(|| self.delivery_order.first())
            .copied()
            .ok_or(SimulationRuntimeError::EmptyDeliveryQueue)
    }
}

/// Violations recorded only when a simulated storm reaches the corresponding
/// production scheduler or actor boundary.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ProtectionObservations {
    violations: BTreeSet<ProtectionLayer>,
    barrier_control_blocked: bool,
    barrier_deadline_miss: Option<DeadlineMissWitness>,
    storage_progress_between_entries: bool,
}

impl ProtectionObservations {
    /// Named propositions violated by production work in this runtime.
    pub(crate) fn violations(&self) -> &BTreeSet<ProtectionLayer> {
        &self.violations
    }

    /// Whether a production reassembly ordering barrier rejected control.
    pub(crate) const fn barrier_control_blocked(&self) -> bool {
        self.barrier_control_blocked
    }

    fn record(&mut self, layer: ProtectionLayer) {
        self.violations.insert(layer);
    }

    fn record_barrier_blocked(&mut self) {
        self.barrier_control_blocked = true;
        if self
            .barrier_deadline_miss
            .is_some_and(|witness| witness.observed_virtual_ms > witness.deadline_virtual_ms)
        {
            self.record(ProtectionLayer::BarrierControlExemption);
        }
    }

    fn record_barrier_deadline_miss(&mut self, witness: DeadlineMissWitness) {
        self.barrier_deadline_miss = Some(witness);
        if self.barrier_control_blocked && witness.observed_virtual_ms > witness.deadline_virtual_ms
        {
            self.record(ProtectionLayer::BarrierControlExemption);
        }
    }

    fn record_storage_progress_between_entries(&mut self) {
        self.storage_progress_between_entries = true;
    }

    /// Whether independently scheduled work ran after one persistence effect
    /// and before the next persistence effect in the same production batch.
    pub(crate) const fn storage_progress_between_entries(&self) -> bool {
        self.storage_progress_between_entries
    }
}

/// Typed failures at the deterministic simulation effect boundary.
#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub(crate) enum SimulationRuntimeError {
    /// A simulation was already installed on this current-thread runtime.
    #[error("a deterministic simulation runtime is already active on this thread")]
    AlreadyActive,
    /// A simulation-only operation escaped its guard.
    #[error("no deterministic simulation runtime is active on this thread")]
    NotActive,
    /// A task-backed simulation observer was requested outside Tokio.
    #[error("deterministic simulation observer requires an active Tokio runtime")]
    MissingTokioRuntime,
    /// Advancing either independent clock would exceed its representation.
    #[error("simulation time overflowed")]
    TimeOverflow,
    /// A delivery choice was requested for an empty queue.
    #[error("cannot choose a delivery from an empty controlled queue")]
    EmptyDeliveryQueue,
    /// A stable delivery sequence was absent from the controlled queue.
    #[error("controlled delivery sequence {sequence} is not queued")]
    UnknownDelivery {
        /// Stable controlled-queue sequence requested by the harness.
        sequence: u64,
    },
    /// A platform-sized delivery count could not be represented by the seeded scheduler.
    #[error("pending delivery count {pending} cannot be represented by the scheduler")]
    PendingDeliveryCountOverflow {
        /// Pending controlled deliveries observed by the harness.
        pending: usize,
    },
    /// A queued real transport payload could not be classified for scheduling.
    #[error("queued delivery {sequence} could not be decoded: {reason}")]
    UndecodableQueuedMessage {
        /// Stable controlled-queue sequence.
        sequence: u64,
        /// Decode failure from the production payload format.
        reason: String,
    },
    /// A production effect was reached without its deterministic simulation adapter.
    #[error("simulation effect `{effect}` escaped its deterministic adapter")]
    ProductionEffectEscape {
        /// Effect boundary that was not configured deterministically.
        effect: &'static str,
    },
}

/// Owns one deterministic simulation runtime on the current OS thread.
///
/// The `Rc` marker makes cross-thread movement unrepresentable. Dropping the
/// guard clears both the core effect overrides and the dummy delivery queue.
pub(crate) struct SimulationRuntimeGuard {
    _exclusive: MutexGuard<'static, ()>,
    _current_thread: PhantomData<Rc<()>>,
}

impl SimulationRuntimeGuard {
    /// Install an isolated deterministic runtime and controlled dummy transport.
    pub(crate) fn enter(
        seed: u64,
        epoch_ms: u128,
        protection: ProtectionProfile,
    ) -> Result<Self, SimulationRuntimeError> {
        if RUNTIME.with(|runtime| runtime.borrow().is_some()) {
            return Err(SimulationRuntimeError::AlreadyActive);
        }
        let exclusive = SIMULATION_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        dummy_controlled::enable(true);
        dummy_controlled::set_seed(seed);
        dummy_controlled::set_virtual_time(0);
        if let Err(error) = verify_effect_boundary() {
            dummy_controlled::enable(false);
            return Err(error);
        }
        RUNTIME.with(|runtime| {
            *runtime.borrow_mut() = Some(SimulationRuntimeState::new(seed, epoch_ms, protection));
        });
        Ok(Self {
            _exclusive: exclusive,
            _current_thread: PhantomData,
        })
    }

    /// Advance virtual monotonic time and the independent simulated epoch clock.
    pub(crate) async fn advance(&self, duration: Duration) -> Result<(), SimulationRuntimeError> {
        verify_effect_boundary()?;
        let now_ms = with_runtime_mut(|runtime| runtime.advance(duration))??;
        dummy_controlled::set_virtual_time(now_ms);
        tokio::time::advance(duration).await;
        tokio::task::yield_now().await;
        Ok(())
    }

    /// Choose one pending controlled delivery from the seeded schedule.
    pub(crate) fn choose_delivery(&self, pending: usize) -> Result<usize, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| runtime.choose_delivery(pending))?
    }

    /// Return elapsed virtual milliseconds without consulting epoch time.
    pub(crate) fn elapsed_ms(&self) -> Result<u128, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.elapsed_ms)
    }

    /// Set the complete scenario identity used by failure artifacts.
    pub(crate) fn set_artifact_identity(
        &self,
        identity: String,
        context: serde_json::Value,
    ) -> Result<(), SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            runtime.artifact_identity = identity;
            runtime.artifact_context = context;
        })
    }

    /// Return the complete scenario identity used by failure artifacts.
    pub(crate) fn artifact_identity(&self) -> Result<String, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.artifact_identity.clone())
    }

    /// Return structured scenario metadata embedded in every failure artifact.
    pub(crate) fn artifact_context(&self) -> Result<serde_json::Value, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.artifact_context.clone())
    }

    /// Select a queued real dummy event according to a reproducible policy.
    pub(crate) fn select_delivery(
        &self,
        strategy: DeliveryStrategy,
    ) -> Result<Option<ScheduledDelivery>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            refresh_delivery_cache(runtime)?;
            if runtime.delivery_order.is_empty() {
                return Ok(None);
            }
            let sequence = match strategy {
                DeliveryStrategy::Fifo => runtime.delivery_order.first().copied(),
                DeliveryStrategy::Lifo => runtime.delivery_order.last().copied(),
                DeliveryStrategy::Seeded => Some(runtime.choose_delivery_sequence()?),
                DeliveryStrategy::AdversarialControlLast => runtime
                    .non_control_delivery_order
                    .first()
                    .or_else(|| runtime.delivery_order.first())
                    .copied(),
            };
            let sequence = sequence.ok_or(SimulationRuntimeError::EmptyDeliveryQueue)?;
            let delivery = runtime
                .delivery_cache
                .get(&sequence)
                .cloned()
                .ok_or(SimulationRuntimeError::UnknownDelivery { sequence })?;
            Ok(Some(delivery))
        })?
    }

    /// Inspect every currently queued event without removing it.
    pub(crate) fn pending_deliveries(
        &self,
    ) -> Result<Vec<ScheduledDelivery>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            refresh_delivery_cache(runtime)?;
            runtime
                .delivery_order
                .iter()
                .map(|sequence| {
                    runtime.delivery_cache.get(sequence).cloned().ok_or(
                        SimulationRuntimeError::UnknownDelivery {
                            sequence: *sequence,
                        },
                    )
                })
                .collect()
        })?
    }

    /// Inspect only queue entries appended since the preceding inspection.
    pub(crate) fn new_pending_deliveries(
        &self,
    ) -> Result<Vec<ScheduledDelivery>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            refresh_delivery_cache(runtime)?;
            let newly_reported = std::mem::take(&mut runtime.unreported_deliveries);
            newly_reported
                .into_iter()
                .map(|sequence| {
                    runtime
                        .delivery_cache
                        .get(&sequence)
                        .cloned()
                        .ok_or(SimulationRuntimeError::UnknownDelivery { sequence })
                })
                .collect()
        })?
    }

    /// Remove and execute the exact controlled delivery selected from this runtime.
    pub(crate) async fn deliver(
        &self,
        delivery: &ScheduledDelivery,
    ) -> Result<bool, SimulationRuntimeError> {
        verify_effect_boundary()?;
        remove_cached_delivery(delivery)?;
        Ok(dummy_controlled::deliver_sequence(delivery.sequence).await)
    }

    /// Remove one controlled event without executing its callback.
    pub(crate) fn discard(
        &self,
        delivery: &ScheduledDelivery,
    ) -> Result<bool, SimulationRuntimeError> {
        verify_effect_boundary()?;
        remove_cached_delivery(delivery)?;
        Ok(dummy_controlled::discard_sequence(delivery.sequence))
    }

    /// Snapshot violations emitted by real production boundaries in this run.
    pub(crate) fn protection_observations(
        &self,
    ) -> Result<ProtectionObservations, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.observations.clone())
    }

    /// Snapshot peaks observed at production admission and reassembly boundaries.
    pub(crate) fn capacity_observations(
        &self,
    ) -> Result<ProductionCapacityObservations, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.capacity_observations.clone())
    }

    /// Number of entries emitted by the production storage-repair planner.
    pub(crate) fn repair_entries_observed(&self) -> Result<u64, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.repair_entries_observed)
    }

    /// Arm a real cooperative-progress observer for the next storage batch.
    pub(crate) fn arm_storage_progress_probe(&self) -> Result<Arc<Notify>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            let notify = Arc::new(Notify::new());
            runtime.storage_progress_notify = Some(notify.clone());
            runtime.storage_progress_epoch = 0;
            notify
        })
    }

    /// Return progress made by the independently scheduled storage observer.
    pub(crate) fn storage_progress_epoch(&self) -> Result<u64, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.storage_progress_epoch)
    }

    /// Disarm the storage progress probe after its bounded pressure scenario.
    pub(crate) fn disarm_storage_progress_probe(&self) -> Result<(), SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| runtime.storage_progress_notify = None)
    }

    /// Drain production handler observations in their real effect order.
    pub(crate) fn take_production_trace_observations(
        &self,
    ) -> Result<Vec<ProductionTraceObservation>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| std::mem::take(&mut runtime.production_trace_observations))
    }

    /// Consume one real accepted-chunk observation for this transaction.
    pub(crate) fn take_reassembly_advance_observed(
        &self,
        transaction_id: uuid::Uuid,
    ) -> Result<bool, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime_mut(|runtime| {
            let Some(count) = runtime.reassembly_advances.get_mut(&transaction_id) else {
                return false;
            };
            *count = count.saturating_sub(1);
            if *count == 0 {
                runtime.reassembly_advances.remove(&transaction_id);
            }
            true
        })
    }

    /// Real production submission time recorded before scheduler admission.
    pub(crate) fn outbound_submission_ms(
        &self,
        transaction_id: uuid::Uuid,
    ) -> Result<Option<u64>, SimulationRuntimeError> {
        verify_effect_boundary()?;
        with_runtime(|runtime| runtime.outbound_submission_ms.get(&transaction_id).copied())
    }
}

fn verify_effect_boundary() -> Result<(), SimulationRuntimeError> {
    if !dummy_controlled::is_enabled() {
        return Err(SimulationRuntimeError::ProductionEffectEscape {
            effect: "dummy-delivery",
        });
    }
    if !dummy_controlled::is_seeded() {
        return Err(SimulationRuntimeError::ProductionEffectEscape {
            effect: "dummy-rng",
        });
    }
    Ok(())
}

impl Drop for SimulationRuntimeGuard {
    fn drop(&mut self) {
        dummy_controlled::enable(false);
        RUNTIME.with(|runtime| {
            *runtime.borrow_mut() = None;
        });
    }
}

/// Return the active simulation profile, or the production profile outside a guard.
pub(crate) fn protection_profile() -> ProtectionProfile {
    RUNTIME.with(|runtime| {
        runtime
            .borrow()
            .as_ref()
            .map_or(ProtectionProfile::ALL_ENABLED, |runtime| runtime.protection)
    })
}

/// Return a deterministic epoch override when a simulation guard is active.
pub(crate) fn epoch_ms_override() -> Option<u128> {
    RUNTIME.with(|runtime| runtime.borrow().as_ref().map(|runtime| runtime.epoch_ms))
}

/// Return the next deterministic UUID when a simulation guard is active.
pub(crate) fn next_uuid_override() -> Option<uuid::Uuid> {
    RUNTIME.with(|runtime| {
        runtime
            .borrow_mut()
            .as_mut()
            .map(SimulationRuntimeState::next_uuid)
    })
}

fn with_runtime<T>(
    operation: impl FnOnce(&SimulationRuntimeState) -> T,
) -> Result<T, SimulationRuntimeError> {
    RUNTIME.with(|runtime| {
        runtime
            .borrow()
            .as_ref()
            .map(operation)
            .ok_or(SimulationRuntimeError::NotActive)
    })
}

fn with_runtime_mut<T>(
    operation: impl FnOnce(&mut SimulationRuntimeState) -> T,
) -> Result<T, SimulationRuntimeError> {
    RUNTIME.with(|runtime| {
        runtime
            .borrow_mut()
            .as_mut()
            .map(operation)
            .ok_or(SimulationRuntimeError::NotActive)
    })
}

fn deterministic_uuid(seed: u64, sequence: u64) -> uuid::Uuid {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&seed.to_be_bytes());
    bytes[8..].copy_from_slice(&sequence.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | UUID_VERSION_4;
    bytes[8] = (bytes[8] & 0x3f) | UUID_RFC4122_VARIANT;
    uuid::Uuid::from_bytes(bytes)
}

/// Derive another deterministic simulation seed from the transport runtime's
/// canonical mixer.
pub(crate) const fn mix_seed(seed: u64) -> u64 {
    dummy_controlled::mix_seed(seed)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::inspect_message;
    use super::DeliveryStrategy;
    use super::ProtectionProfile;
    use super::ScheduledDelivery;
    use super::ScheduledDeliveryClass;
    use super::SimulationRuntimeError;
    use super::SimulationRuntimeGuard;
    use crate::chunk::Chunk;
    use crate::chunk::ChunkMeta;
    use crate::dht::StorageSyncDestination;
    use crate::dht::StorageSyncPurpose;
    use crate::ecc::SecretKey;
    use crate::message::Message;
    use crate::message::MessagePayload;
    use crate::message::PeerLivenessProbe;
    use crate::message::SyncEntriesWithSuccessor;
    use crate::session::SessionSk;

    #[tokio::test(start_paused = true)]
    async fn same_seed_replays_clock_uuid_and_delivery_choices() {
        let first = run_effect_trace(17).await;
        let second = run_effect_trace(17).await;
        assert_eq!(first, second);
    }

    #[test]
    fn production_wire_classification_distinguishes_control_storage_and_chunks() {
        let guard = SimulationRuntimeGuard::enter(3, 100, ProtectionProfile::ALL_ENABLED)
            .expect("runtime must install");
        let session =
            SessionSk::new_with_seckey(&SecretKey::random()).expect("test session must be valid");
        let did = session.account_did();
        let fixtures = [
            (
                Message::PeerLivenessProbe(PeerLivenessProbe { sent_at_ms: 1 }),
                ScheduledDeliveryClass::Control,
            ),
            (
                Message::SyncEntriesWithSuccessor(SyncEntriesWithSuccessor {
                    purpose: StorageSyncPurpose::AdditiveRepair,
                    destination: StorageSyncDestination::PhysicalOwner(did),
                    data: Vec::new(),
                }),
                ScheduledDeliveryClass::Storage,
            ),
            (
                Message::Chunk(Chunk {
                    chunk: [0, 1],
                    data: Bytes::from_static(b"chunk"),
                    meta: ChunkMeta::default(),
                }),
                ScheduledDeliveryClass::Reassembly,
            ),
            (
                Message::custom(b"application").expect("custom message must encode"),
                ScheduledDeliveryClass::Application,
            ),
        ];
        for (sequence, (message, expected)) in fixtures.into_iter().enumerate() {
            let wire = MessagePayload::new_send(message, &session, did, did)
                .and_then(|payload| payload.to_wire())
                .expect("payload must encode");
            assert_eq!(
                inspect_message(sequence as u64, &wire)
                    .expect("payload must classify")
                    .0,
                expected
            );
        }
        for strategy in [
            DeliveryStrategy::Fifo,
            DeliveryStrategy::Lifo,
            DeliveryStrategy::Seeded,
            DeliveryStrategy::AdversarialControlLast,
        ] {
            assert!(guard
                .select_delivery(strategy)
                .expect("empty queue selection is valid")
                .is_none());
        }
        assert!(guard
            .pending_deliveries()
            .expect("empty queue inspection is valid")
            .is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn nested_runtime_is_rejected_with_a_typed_error() {
        let guard = SimulationRuntimeGuard::enter(1, 10, ProtectionProfile::ALL_ENABLED)
            .expect("first runtime must install");
        let nested = SimulationRuntimeGuard::enter(2, 20, ProtectionProfile::ALL_ENABLED);
        assert!(matches!(nested, Err(SimulationRuntimeError::AlreadyActive)));
        drop(guard);
    }

    #[test]
    fn one_reassembly_accept_cannot_be_reused_by_a_replayed_occurrence() {
        let guard = SimulationRuntimeGuard::enter(4, 100, ProtectionProfile::ALL_ENABLED)
            .expect("runtime must install");
        let transaction_id = uuid::Uuid::from_u128(7);
        super::record_reassembly_advance(transaction_id);

        assert!(guard
            .take_reassembly_advance_observed(transaction_id)
            .expect("accepted occurrence must remain visible"));
        assert!(!guard
            .take_reassembly_advance_observed(transaction_id)
            .expect("same-UUID replay must not reuse the accepted occurrence"));
    }

    #[test]
    fn stale_delivery_identity_reports_the_unknown_sequence() {
        let guard = SimulationRuntimeGuard::enter(6, 100, ProtectionProfile::ALL_ENABLED)
            .expect("runtime must install");
        let stale = ScheduledDelivery {
            sequence: 41,
            connection_generation: "retired-generation".to_owned(),
            transaction_id: None,
            class: ScheduledDeliveryClass::Lifecycle,
            bytes: 0,
            enqueued_virtual_ms: 0,
            deadline_virtual_ms: None,
        };

        assert!(matches!(
            guard.discard(&stale),
            Err(SimulationRuntimeError::UnknownDelivery { sequence: 41 })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn runtime_sentinel_rejects_an_uncontrolled_dummy_escape() {
        let guard = SimulationRuntimeGuard::enter(5, 10, ProtectionProfile::ALL_ENABLED)
            .expect("runtime must install");
        rings_transport::connections::dummy_controlled::enable(false);
        assert!(matches!(
            guard.pending_deliveries(),
            Err(SimulationRuntimeError::ProductionEffectEscape {
                effect: "dummy-delivery"
            })
        ));
        rings_transport::connections::dummy_controlled::enable(true);
        rings_transport::connections::dummy_controlled::set_seed(5);
    }

    #[test]
    fn protection_profiles_change_exactly_the_named_layer() {
        let enabled = ProtectionProfile::ALL_ENABLED;
        let cases = [
            ProtectionProfile::without_class_reservations(),
            ProtectionProfile::without_bounded_control_burst(),
            ProtectionProfile::without_barrier_control_exemption(),
            ProtectionProfile::without_per_entry_yield(),
            ProtectionProfile::LEGACY_ALL_DISABLED,
        ];
        let observations = cases.map(|profile| {
            (
                profile.class_reservations(),
                profile.bounded_control_burst(),
                profile.barrier_control_exemption(),
                profile.per_entry_yield(),
            )
        });
        assert_eq!(observations, [
            (false, true, true, true),
            (true, false, true, true),
            (true, true, false, true),
            (true, true, true, false),
            (false, false, false, false),
        ]);
        assert_eq!(enabled, ProtectionProfile::ALL_ENABLED);
    }

    async fn run_effect_trace(seed: u64) -> (u128, u128, uuid::Uuid, uuid::Uuid, Vec<usize>) {
        let guard =
            SimulationRuntimeGuard::enter(seed, 1_700_000_000_000, ProtectionProfile::ALL_ENABLED)
                .expect("runtime must install");
        let first = crate::utils::new_uuid();
        guard
            .advance(std::time::Duration::from_millis(25))
            .await
            .expect("virtual time must advance");
        let second = crate::utils::new_uuid();
        let choices = (1..=8)
            .map(|pending| {
                guard
                    .choose_delivery(pending)
                    .expect("non-empty queue must be selectable")
            })
            .collect();
        let trace = (
            crate::utils::get_epoch_ms(),
            guard.elapsed_ms().expect("elapsed time must be observable"),
            first,
            second,
            choices,
        );
        drop(guard);
        trace
    }
}
