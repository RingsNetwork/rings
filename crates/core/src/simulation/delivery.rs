//! Incremental controlled-delivery inspection and production wire classification.

use std::collections::BTreeMap;

use rings_transport::connections::dummy_controlled;
use rings_transport::connections::dummy_controlled::QueuedDeliveryKind;
use rings_transport::core::pool::ChannelLane;

use super::with_runtime_mut;
use super::SimulationRuntimeError;
use super::SimulationRuntimeState;
use super::CONTROL_DEADLINE_MS;
use crate::message::LinkFrame;
use crate::message::MessageCategory;
use crate::message::MessageKind;

/// Delivery classes visible to deterministic schedule policies.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) enum ScheduledDeliveryClass {
    /// Connection or data-channel lifecycle callback.
    Lifecycle,
    /// Chord or liveness control payload.
    Control,
    /// A link-control frame of the delegation references (a lease or a disclaimer): a frame of
    /// the link itself, belonging to no transaction and to no transfer lane. A delayed lease or
    /// disclaimer costs inline frames, never a stall, so it carries no deadline.
    LinkControl,
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
    /// The transport lane a message travels on; `None` for a lifecycle event.
    #[serde(skip)]
    pub(crate) lane: Option<ChannelLane>,
}

/// Admit the controlled events queued since the last inspection into the runtime's view.
///
/// Law (lane order). The transport delivers one lane's messages in their send order
/// (`rings_transport::core::pool`), so a schedule may select a message only at the head of its
/// connection's lane; a lifecycle event belongs to no lane and is always selectable:
///
/// ```text
/// selectable(d) ⟺ lane(d) = ⊥ ∨ d = min { e pending | conn(e) = conn(d) ∧ lane(e) = lane(d) }
/// ```
pub(super) fn refresh_delivery_cache(
    runtime: &mut SimulationRuntimeState,
) -> Result<Vec<u64>, SimulationRuntimeError> {
    let queued = dummy_controlled::inspect_after(runtime.last_inspected_delivery);
    let mut added = Vec::with_capacity(queued.len());
    for queued in queued {
        let delivery = inspect_delivery(queued, &runtime.outbound_submission_ms)?;
        let sequence = delivery.sequence;
        runtime.last_inspected_delivery = Some(sequence);
        runtime.delivery_order.insert(sequence);
        runtime.unreported_deliveries.insert(sequence);
        added.push(sequence);
        let head = delivery.lane.is_none_or(|lane| {
            let queue = runtime
                .lane_queues
                .entry((delivery.connection_generation.clone(), lane))
                .or_default();
            queue.insert(sequence);
            queue.len() == 1
        });
        if head {
            mark_selectable(runtime, &delivery);
        }
        runtime.delivery_cache.insert(sequence, delivery);
    }
    Ok(added)
}

/// Make `delivery` selectable by every strategy, and by the non-control ones unless it is
/// control.
fn mark_selectable(runtime: &mut SimulationRuntimeState, delivery: &ScheduledDelivery) {
    runtime.selectable_order.insert(delivery.sequence);
    if delivery.class != ScheduledDeliveryClass::Control {
        runtime
            .non_control_selectable_order
            .insert(delivery.sequence);
    }
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
        runtime.selectable_order.remove(&delivery.sequence);
        runtime
            .non_control_selectable_order
            .remove(&delivery.sequence);
        runtime.unreported_deliveries.remove(&delivery.sequence);
        runtime.delivery_cache.remove(&delivery.sequence);
        if let Some(lane) = delivery.lane {
            let key = (delivery.connection_generation.clone(), lane);
            let next = runtime.lane_queues.get_mut(&key).and_then(|queue| {
                queue.remove(&delivery.sequence);
                queue.first().copied()
            });
            match next.and_then(|next| runtime.delivery_cache.get(&next).cloned()) {
                Some(next) => mark_selectable(runtime, &next),
                None => {
                    runtime.lane_queues.remove(&key);
                }
            }
        }
        Ok(())
    })?
}

fn inspect_delivery(
    queued: rings_transport::connections::dummy_controlled::QueuedDelivery,
    outbound_submission_ms: &BTreeMap<uuid::Uuid, u64>,
) -> Result<ScheduledDelivery, SimulationRuntimeError> {
    let (class, bytes, transaction_id, lane) = match queued.kind() {
        QueuedDeliveryKind::PeerConnectionStateChange(_)
        | QueuedDeliveryKind::DataChannelOpen
        | QueuedDeliveryKind::DataChannelClose => {
            (ScheduledDeliveryClass::Lifecycle, 0, None, None)
        }
        QueuedDeliveryKind::Message(bytes, lane) => {
            let (class, transaction_id) = inspect_message(queued.sequence(), bytes)?;
            (class, bytes.len(), transaction_id, Some(*lane))
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
        lane,
    })
}

/// Classify one queued frame as the production link would, without resolving its delegation slots:
/// the class and transaction id of a payload are readable whatever its slots hold, and a
/// link-control frame is the link's own, belonging to no transaction.
pub(super) fn inspect_message(
    sequence: u64,
    bytes: &[u8],
) -> Result<(ScheduledDeliveryClass, Option<uuid::Uuid>), SimulationRuntimeError> {
    let frame = LinkFrame::from_wire(bytes).map_err(|error| {
        SimulationRuntimeError::UndecodableQueuedMessage {
            sequence,
            reason: error.to_string(),
        }
    })?;
    let payload = match frame {
        LinkFrame::Control(_) => return Ok((ScheduledDeliveryClass::LinkControl, None)),
        LinkFrame::Payload(payload) => payload,
    };
    let transaction_id = Some(payload.transaction_id());
    let kind = MessageKind::from_wire(payload.transaction_data()).map_err(|error| {
        SimulationRuntimeError::UndecodableQueuedMessage {
            sequence,
            reason: error.to_string(),
        }
    })?;
    if kind.is_chunk() {
        return Ok((ScheduledDeliveryClass::Reassembly, transaction_id));
    }
    let class = match kind.class() {
        MessageCategory::DhtControl => ScheduledDeliveryClass::Control,
        MessageCategory::Storage => ScheduledDeliveryClass::Storage,
        MessageCategory::E2e => ScheduledDeliveryClass::E2e,
        MessageCategory::Application => ScheduledDeliveryClass::Application,
    };
    Ok((class, transaction_id))
}
