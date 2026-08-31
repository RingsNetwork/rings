//! Incremental controlled-delivery inspection and production wire classification.

use std::collections::BTreeMap;

use rings_transport::connections::dummy_controlled;
use rings_transport::connections::dummy_controlled::QueuedDeliveryKind;

use super::with_runtime_mut;
use super::SimulationRuntimeError;
use super::SimulationRuntimeState;
use super::CONTROL_DEADLINE_MS;
use crate::message::MessageClass;
use crate::message::MessageKind;
use crate::message::MessagePayload;

/// Delivery classes visible to deterministic schedule policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) enum ScheduledDeliveryClass {
    /// Connection or data-channel lifecycle callback.
    Lifecycle,
    /// Chord or liveness control payload.
    Control,
    /// Storage synchronization payload.
    Storage,
    /// One chunk frame requiring reassembly.
    Reassembly,
    /// End-to-end encrypted payload.
    E2e,
    /// Application payload.
    Application,
}

/// Reproducible policies for selecting the next real dummy-transport event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryStrategy {
    /// Oldest queued event first.
    Fifo,
    /// Newest queued event first.
    Lifo,
    /// Seeded selection owned by the deterministic runtime guard.
    Seeded,
    /// Prefer non-control work to maximize control latency reproducibly.
    AdversarialControlLast,
}

impl DeliveryStrategy {
    /// Stable name used by trace artifacts and replay commands.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Fifo => "fifo",
            Self::Lifo => "lifo",
            Self::Seeded => "seeded",
            Self::AdversarialControlLast => "adversarial-control-last",
        }
    }
}

/// Stable metadata for a queued event selected by the simulation scheduler.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct ScheduledDelivery {
    /// Monotonic enqueue sequence, stable across removals.
    pub(crate) sequence: u64,
    /// Stable dummy connection generation receiving this event.
    pub(crate) connection_generation: String,
    /// Production transaction identifier decoded from message bytes.
    pub(crate) transaction_id: Option<uuid::Uuid>,
    /// Semantic class decoded through the production wire format.
    pub(crate) class: ScheduledDeliveryClass,
    /// Exact core payload bytes, or zero for lifecycle events.
    pub(crate) bytes: usize,
    /// Virtual monotonic production submission time, including admission and
    /// scheduler waiting; lifecycle events fall back to dummy enqueue time.
    pub(crate) enqueued_virtual_ms: u64,
    /// Virtual deadline for control delivery, when this class has one.
    pub(crate) deadline_virtual_ms: Option<u64>,
}

pub(super) fn refresh_delivery_cache(
    runtime: &mut SimulationRuntimeState,
) -> Result<Vec<u64>, SimulationRuntimeError> {
    let queued = dummy_controlled::inspect_after(runtime.last_inspected_delivery);
    let mut added = Vec::with_capacity(queued.len());
    for queued in queued {
        let delivery = inspect_delivery(queued, &runtime.outbound_submission_ms)?;
        runtime.last_inspected_delivery = Some(delivery.sequence);
        runtime.delivery_order.insert(delivery.sequence);
        if delivery.class != ScheduledDeliveryClass::Control {
            runtime.non_control_delivery_order.insert(delivery.sequence);
        }
        runtime.unreported_deliveries.insert(delivery.sequence);
        added.push(delivery.sequence);
        runtime.delivery_cache.insert(delivery.sequence, delivery);
    }
    Ok(added)
}

pub(super) fn remove_cached_delivery(
    delivery: &ScheduledDelivery,
) -> Result<(), SimulationRuntimeError> {
    with_runtime_mut(|runtime| {
        refresh_delivery_cache(runtime)?;
        if !runtime.delivery_order.remove(&delivery.sequence) {
            return Err(SimulationRuntimeError::UnknownDelivery {
                sequence: delivery.sequence,
            });
        }
        runtime
            .non_control_delivery_order
            .remove(&delivery.sequence);
        runtime.unreported_deliveries.remove(&delivery.sequence);
        runtime.delivery_cache.remove(&delivery.sequence);
        Ok(())
    })?
}

fn inspect_delivery(
    queued: rings_transport::connections::dummy_controlled::QueuedDelivery,
    outbound_submission_ms: &BTreeMap<uuid::Uuid, u64>,
) -> Result<ScheduledDelivery, SimulationRuntimeError> {
    let (class, bytes, transaction_id) = match queued.kind() {
        QueuedDeliveryKind::PeerConnectionStateChange(_)
        | QueuedDeliveryKind::DataChannelOpen
        | QueuedDeliveryKind::DataChannelClose => (ScheduledDeliveryClass::Lifecycle, 0, None),
        QueuedDeliveryKind::Message(bytes) => {
            let (class, transaction_id) = inspect_message(queued.sequence(), bytes)?;
            (class, bytes.len(), Some(transaction_id))
        }
    };
    let enqueued_virtual_ms = transaction_id
        .and_then(|transaction_id| outbound_submission_ms.get(&transaction_id).copied())
        .unwrap_or_else(|| queued.enqueued_virtual_ms());
    Ok(ScheduledDelivery {
        sequence: queued.sequence(),
        connection_generation: queued.connection_id().to_string(),
        transaction_id,
        class,
        bytes,
        enqueued_virtual_ms,
        deadline_virtual_ms: (class == ScheduledDeliveryClass::Control)
            .then(|| enqueued_virtual_ms.saturating_add(CONTROL_DEADLINE_MS)),
    })
}

pub(super) fn inspect_message(
    sequence: u64,
    bytes: &[u8],
) -> Result<(ScheduledDeliveryClass, uuid::Uuid), SimulationRuntimeError> {
    let payload = MessagePayload::from_wire(bytes).map_err(|error| {
        SimulationRuntimeError::UndecodableQueuedMessage {
            sequence,
            reason: error.to_string(),
        }
    })?;
    let transaction_id = payload.transaction.tx_id;
    let kind = MessageKind::from_wire(&payload.transaction.data).map_err(|error| {
        SimulationRuntimeError::UndecodableQueuedMessage {
            sequence,
            reason: error.to_string(),
        }
    })?;
    if kind.is_chunk() {
        return Ok((ScheduledDeliveryClass::Reassembly, transaction_id));
    }
    let class = match kind.class() {
        MessageClass::DhtControl => ScheduledDeliveryClass::Control,
        MessageClass::Storage => ScheduledDeliveryClass::Storage,
        MessageClass::E2e => ScheduledDeliveryClass::E2e,
        MessageClass::Application => ScheduledDeliveryClass::Application,
    };
    Ok((class, transaction_id))
}
