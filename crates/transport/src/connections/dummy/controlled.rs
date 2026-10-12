use std::sync::Arc;

use bytes::Bytes;

pub use super::delay::mix_seed;
use super::ACTIVE_DELIVERY_GATE;
use super::CLOSE_PENDING;
use super::CONNS;
use super::CONTROLLED;
use super::CONTROLLED_RNG_STATE;
use super::CONTROLLED_VIRTUAL_MS;
use super::DELIVERY;
use super::DELIVERY_ENQUEUED;
use super::DELIVERY_FUTURE_PENDING;
use super::DROP_MESSAGES;
use super::HELD_DELIVERY_GATE;
use super::IRREVOCABLE_SEND_GATE;
use super::IRREVOCABLE_SEND_GATE_WAITING;
use super::MAX_MESSAGE_SIZE;
use super::NEXT_CALLBACK_CID;
use super::NEXT_DELIVERY_GATE;
use super::POST_PERMIT_SEND_GATE;
use super::POST_PERMIT_SEND_GATE_WAITING;
use super::SEND_MESSAGE_GATE;
use super::SEND_MESSAGE_GATE_WAITING;
use super::SEND_MESSAGE_PENDING;
use super::SEND_MESSAGE_PENDING_AFTER_SENT_COUNT;
use super::WAIT_FOR_DATA_CHANNEL_OPEN_PENDING;
use super::WITHHELD_CREDIT;
use crate::core::transport::WebrtcConnectionState;

/// Atomic observation of the current thread's controlled delivery queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliverySnapshot {
    pending: usize,
    generation: u64,
}

impl DeliverySnapshot {
    pub(super) const fn new(pending: usize, generation: u64) -> Self {
        Self {
            pending,
            generation,
        }
    }

    /// Return whether no controlled event is currently queued.
    pub const fn is_idle(self) -> bool {
        self.pending == 0
    }

    /// Return the number of controlled events currently queued.
    pub const fn pending(self) -> usize {
        self.pending
    }

    /// Return the queue generation, advanced on every enqueue or removal.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// Stable observation of one event waiting in the controlled queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedDelivery {
    sequence: u64,
    connection_id: String,
    kind: QueuedDeliveryKind,
    enqueued_virtual_ms: u64,
}

impl QueuedDelivery {
    pub(super) fn new(
        sequence: u64,
        connection_id: String,
        kind: QueuedDeliveryKind,
        enqueued_virtual_ms: u64,
    ) -> Self {
        Self {
            sequence,
            connection_id,
            kind,
            enqueued_virtual_ms,
        }
    }

    /// Monotonic enqueue sequence within the active controlled runtime.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Dummy connection identifier receiving this event.
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    /// Semantic event kind and message bytes, when this is a message event.
    pub const fn kind(&self) -> &QueuedDeliveryKind {
        &self.kind
    }

    /// Virtual monotonic time at which this event entered the controlled queue.
    pub const fn enqueued_virtual_ms(&self) -> u64 {
        self.enqueued_virtual_ms
    }
}

/// Observable event kinds retained by the controlled dummy scheduler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueuedDeliveryKind {
    /// A WebRTC state transition.
    PeerConnectionStateChange(WebrtcConnectionState),
    /// The data channel became writable.
    DataChannelOpen,
    /// The data channel closed.
    DataChannelClose,
    /// One exact callback payload, before core decoding and dispatch.
    Message(Bytes),
}

/// Turn the controlled scheduler on/off for the current thread. Turning it
/// off clears this thread's queue.
pub fn enable(on: bool) {
    CONTROLLED.with(|c| c.set(on));
    if on {
        DELIVERY.with(|state| state.borrow_mut().reset());
    } else {
        DELIVERY.with(|state| state.borrow_mut().clear());
        super::SENT_COUNT.with(|count| count.set(0));
        MAX_MESSAGE_SIZE.with(|size| size.set(0));
        NEXT_CALLBACK_CID.with(|next| {
            *next.borrow_mut() = None;
        });
        WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.set(false));
        SEND_MESSAGE_PENDING.with(|pending| pending.set(false));
        release_send_message_gate();
        release_post_permit_send_gate();
        release_irrevocable_send_gate();
        SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|threshold| threshold.set(None));
        DELIVERY_FUTURE_PENDING.with(|pending| pending.set(false));
        CLOSE_PENDING.with(|pending| pending.set(false));
        release_delivery_future_gate();
        DROP_MESSAGES.with(|drop| drop.set(false));
        CONTROLLED_RNG_STATE.with(|state| state.set(None));
        CONTROLLED_VIRTUAL_MS.with(|time| time.set(0));
    }
}

/// Seed dummy connection identifiers used by a controlled simulation.
pub fn set_seed(seed: u64) {
    CONTROLLED_RNG_STATE.with(|state| state.set(Some(seed)));
}
/// Set the virtual monotonic clock attached to subsequent queue admissions.
pub fn set_virtual_time(now_ms: u64) {
    CONTROLLED_VIRTUAL_MS.with(|time| time.set(now_ms));
}

/// Whether explicit controlled delivery is active on this test thread.
pub fn is_enabled() -> bool {
    CONTROLLED.with(|controlled| controlled.get())
}

/// Whether dummy identifiers and delay choices have a deterministic seed.
pub fn is_seeded() -> bool {
    CONTROLLED_RNG_STATE.with(|state| state.get().is_some())
}
/// Whether the process-wide registry retains this connection generation.
pub fn is_connection_registered(id: &str) -> bool {
    CONNS.contains_key(id)
}

/// Test hook: override the `max_message_size` the dummy backend reports on this thread (`0`
/// restores the default). Lets a test drive the chunked send path and reassembly end to end.
pub fn set_max_message_size(n: usize) {
    MAX_MESSAGE_SIZE.with(|m| m.set(n));
}

/// Test hook: rewrite the next queued lifecycle callback to use `cid`.
///
/// This applies only to peer-state and data-channel events delivered through
/// [`deliver`]. Message events keep their real connection id.
pub fn set_next_callback_cid(cid: impl Into<String>) {
    NEXT_CALLBACK_CID.with(|next| {
        *next.borrow_mut() = Some(cid.into());
    });
}

/// Test hook: force `webrtc_wait_for_data_channel_open` on this thread to never complete.
pub fn set_wait_for_data_channel_open_pending(on: bool) {
    WAIT_FOR_DATA_CHANNEL_OPEN_PENDING.with(|pending| pending.set(on));
}

/// Test hook: force `send_message` to stay pending after the data channel is open.
pub fn set_send_message_pending(on: bool) {
    SEND_MESSAGE_PENDING.with(|pending| pending.set(on));
}

/// Test hook: suspend the next dummy send immediately before dispatch.
pub fn pause_send_message_at_dispatch() {
    SEND_MESSAGE_GATE.with(|gate| {
        *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
    });
}

/// Test hook: release a send suspended by [`pause_send_message_at_dispatch`].
pub fn release_send_message_gate() {
    let gate = SEND_MESSAGE_GATE.with(|gate| gate.borrow_mut().take());
    if let Some(gate) = gate {
        gate.notify_waiters();
    }
    SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.set(false));
}

/// Return whether a dummy send reached the releasable dispatch gate.
pub fn send_message_waiting_at_dispatch() -> bool {
    SEND_MESSAGE_GATE_WAITING.with(|waiting| waiting.get())
}

/// Test hook: suspend the next dummy send after its initial permit check but
/// before the final cancellable check.
pub fn pause_send_message_after_permit() {
    POST_PERMIT_SEND_GATE.with(|gate| {
        *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
    });
}

/// Test hook: release a send suspended before its final cancellable check.
pub fn release_post_permit_send_gate() {
    let gate = POST_PERMIT_SEND_GATE.with(|gate| gate.borrow_mut().take());
    if let Some(gate) = gate {
        gate.notify_waiters();
    }
    POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
}

/// Return whether a send is suspended before its final cancellable check.
pub fn post_permit_send_gate_waiting() -> bool {
    POST_PERMIT_SEND_GATE_WAITING.with(|waiting| waiting.get())
}

/// Test hook: suspend the next dummy send after its final cancellable boundary.
pub fn pause_irrevocable_send() {
    IRREVOCABLE_SEND_GATE.with(|gate| {
        *gate.borrow_mut() = Some(Arc::new(tokio::sync::Notify::new()));
    });
}

/// Test hook: release a send suspended after it became irrevocable.
pub fn release_irrevocable_send_gate() {
    let gate = IRREVOCABLE_SEND_GATE.with(|gate| gate.borrow_mut().take());
    if let Some(gate) = gate {
        gate.notify_waiters();
    } else {
        IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.set(false));
    }
}

/// Return whether a background dummy send is waiting past its irrevocable boundary.
pub fn irrevocable_send_gate_waiting() -> bool {
    IRREVOCABLE_SEND_GATE_WAITING.with(|waiting| waiting.get())
}

/// Test hook: force `send_message` to stay pending once this thread has already dispatched
/// `threshold` messages. `None` disables the hook.
pub fn set_send_message_pending_after_sent_count(threshold: Option<usize>) {
    SEND_MESSAGE_PENDING_AFTER_SENT_COUNT.with(|pending_after| pending_after.set(threshold));
}

/// Test hook: make an accepted send return a delivery future that never completes.
pub fn set_delivery_future_pending(on: bool) {
    DELIVERY_FUTURE_PENDING.with(|pending| pending.set(on));
}

/// Test hook: make connection cleanup never complete.
pub fn set_close_pending(on: bool) {
    CLOSE_PENDING.with(|pending| pending.set(on));
}

/// Hold the delivery future of every send accepted from now on until
/// [`release_held_delivery_futures`], so a whole in-flight window stays pending.
pub fn hold_delivery_futures() {
    HELD_DELIVERY_GATE.with(|slot| {
        *slot.borrow_mut() = Some(Arc::new(super::HeldDeliveries::new()));
    });
}

/// Delivery futures currently parked by [`hold_delivery_futures`].
pub fn held_delivery_futures_waiting() -> usize {
    HELD_DELIVERY_GATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or(0, |gate| gate.waiting.load(super::Ordering::Acquire))
    })
}

/// Let exactly one delivery future held by [`hold_delivery_futures`] complete.
pub fn release_one_held_delivery_future() {
    if let Some(gate) = HELD_DELIVERY_GATE.with(|slot| slot.borrow().clone()) {
        gate.release_one();
    }
}

/// Release every delivery future held by [`hold_delivery_futures`] and stop holding.
pub fn release_held_delivery_futures() {
    if let Some(gate) = HELD_DELIVERY_GATE.with(|slot| slot.borrow_mut().take()) {
        gate.release();
    }
}

/// Suspend exactly the next accepted send's delivery future.
pub fn pause_next_delivery_future() {
    NEXT_DELIVERY_GATE.with(|slot| {
        *slot.borrow_mut() = Some(Arc::new(super::DeliveryGate::new()));
    });
}

/// Return whether the one-shot delivery future reached its gate.
pub fn delivery_future_waiting() -> bool {
    ACTIVE_DELIVERY_GATE.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|gate| gate.waiting.load(super::Ordering::Acquire))
    })
}

/// Release a delivery future suspended by [`pause_next_delivery_future`].
pub fn release_delivery_future_gate() {
    let gate = ACTIVE_DELIVERY_GATE
        .with(|slot| slot.borrow_mut().take())
        .or_else(|| NEXT_DELIVERY_GATE.with(|slot| slot.borrow_mut().take()));
    if let Some(gate) = gate {
        gate.notify.notify_one();
    }
}

/// Test hook: the connection generation `generation_id` grants its peer no more credit, as a
/// receiver that withholds it: what its peer may send is what it was granted already.
///
/// The set is this thread's, as every dummy hook's: it governs the credit pumps that run on the
/// calling thread, so a test uses it on a current-thread runtime, where every pump does.
pub fn withhold_credit(generation_id: &str) {
    WITHHELD_CREDIT.with(|withheld| withheld.borrow_mut().insert(generation_id.to_string()));
}

/// Test hook: make dummy sends disappear while still returning a successful
/// local send. This models a silent remote failure where the local data
/// channel remains open and `Connected`.
pub fn set_drop_messages(on: bool) {
    DROP_MESSAGES.with(|drop| drop.set(on));
}

/// Test hook: number of data-channel messages `send_message` has dispatched on this thread.
/// Paired with [`reset_sent_count`] to assert that a failed send enqueued nothing.
pub fn sent_count() -> usize {
    super::SENT_COUNT.with(|c| c.get())
}

/// Test hook: reset the [`sent_count`] counter for this thread.
pub fn reset_sent_count() {
    super::SENT_COUNT.with(|c| c.set(0));
}

/// The signal this thread's queue notifies (`notify_waiters`) whenever an event joins it.
///
/// Only a registered waiter is woken, so a test enables its `notified()` future before it
/// polls whatever may enqueue, and then misses no event.
pub fn enqueue_signal() -> Arc<tokio::sync::Notify> {
    DELIVERY_ENQUEUED.with(Arc::clone)
}

/// Number of events currently queued on the current thread.
pub fn pending() -> usize {
    snapshot().pending()
}

/// Atomically observe queue depth and lifecycle generation on the current thread.
pub fn snapshot() -> DeliverySnapshot {
    DELIVERY.with(|state| state.borrow().snapshot())
}

/// Inspect events with a stable sequence newer than `sequence`.
pub fn inspect_after(sequence: Option<u64>) -> Vec<QueuedDelivery> {
    DELIVERY.with(|state| state.borrow().inspect_after(sequence))
}

/// Remove one queued event by stable sequence without invoking its callback.
pub fn discard_sequence(sequence: u64) -> bool {
    DELIVERY.with(|state| state.borrow_mut().remove_sequence(sequence).is_some())
}

/// Deliver the queued event at `index` to its target connection — invoking
/// the real handler, which may enqueue further events. Returns false if the
/// index is out of range or the target connection is gone.
pub async fn deliver(index: usize) -> bool {
    let entry = DELIVERY.with(|state| state.borrow_mut().remove(index));
    deliver_entry(entry).await
}

/// Deliver a queued event by stable sequence in logarithmic queue time.
pub async fn deliver_sequence(sequence: u64) -> bool {
    let entry = DELIVERY.with(|state| state.borrow_mut().remove_sequence(sequence));
    deliver_entry(entry).await
}

async fn deliver_entry(entry: Option<super::ControlledDeliveryEntry>) -> bool {
    let Some(super::ControlledDeliveryEntry {
        connection_id: rand_id,
        mut event,
        ..
    }) = entry
    else {
        return false;
    };
    let Some(conn) = CONNS.get(&rand_id).map(|c| c.clone()) else {
        return false;
    };
    if event.is_lifecycle_event() {
        if let Some(cid) = NEXT_CALLBACK_CID.with(|next| next.borrow_mut().take()) {
            event.set_callback_cid(cid);
        }
    }
    conn.handle_event(event).await;
    true
}

/// Deliver the next queued data-channel-open event with a rewritten callback cid.
pub async fn deliver_next_data_channel_open_with_cid(cid: impl Into<String>) -> bool {
    let index = DELIVERY.with(|state| {
        state
            .borrow()
            .queue
            .values()
            .position(|entry| matches!(entry.event, super::Event::DataChannelOpen(_)))
    });
    let Some(index) = index else {
        return false;
    };
    set_next_callback_cid(cid);
    deliver(index).await
}
