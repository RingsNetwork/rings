//! Per-thread state for deterministic dummy-transport tests.
//!
//! Every cell is thread-local deliberately: a current-thread Tokio test owns
//! its queue, gates, counters, virtual clock, and seeded RNG without changing
//! dummy behavior in tests running concurrently on other OS threads.

use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::Notify;

use super::controlled;
use super::DeliveryGate;
use super::Event;

thread_local! {
    /// Per-(test-)thread controlled-delivery state. Thread-local on purpose: a
    /// current-thread Tokio test owns its queue without affecting parallel tests.
    pub(super) static CONTROLLED: Cell<bool> = const { Cell::new(false) };
    /// Ordered callbacks withheld for explicit delivery by the owning test.
    pub(super) static DELIVERY: RefCell<ControlledDeliveryState> = const {
        RefCell::new(ControlledDeliveryState::new())
    };
    /// Messages accepted by this thread's dummy backend.
    pub(super) static SENT_COUNT: Cell<usize> = const { Cell::new(0) };
    /// Per-test negotiated message-size override; zero restores the default.
    pub(super) static MAX_MESSAGE_SIZE: Cell<usize> = const { Cell::new(0) };
    /// Connection generation assigned to the next callback construction.
    pub(super) static NEXT_CALLBACK_CID: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Force the next data-channel-open waiter to remain pending.
    pub(super) static WAIT_FOR_DATA_CHANNEL_OPEN_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Force sends to remain pending before they enter the data channel.
    pub(super) static SEND_MESSAGE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Optional notification gate before data-channel admission.
    pub(super) static SEND_MESSAGE_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a send is currently parked at [`SEND_MESSAGE_GATE`].
    pub(super) static SEND_MESSAGE_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Optional gate after acquiring a send permit but before publication.
    pub(super) static POST_PERMIT_SEND_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a send is parked after acquiring its permit.
    pub(super) static POST_PERMIT_SEND_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Optional gate at the irrevocable data-channel publication boundary.
    pub(super) static IRREVOCABLE_SEND_GATE: RefCell<Option<Arc<Notify>>> = const { RefCell::new(None) };
    /// Whether a send is parked at the irrevocable publication boundary.
    pub(super) static IRREVOCABLE_SEND_GATE_WAITING: Cell<bool> = const { Cell::new(false) };
    /// Park sends after this thread has published the configured message count.
    pub(super) static SEND_MESSAGE_PENDING_AFTER_SENT_COUNT: Cell<Option<usize>> = const { Cell::new(None) };
    /// Force the receiver-facing delivery completion future to stay pending.
    pub(super) static DELIVERY_FUTURE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// Force connection close to remain pending for cancellation tests.
    pub(super) static CLOSE_PENDING: Cell<bool> = const { Cell::new(false) };
    /// One-shot completion gate installed on the next controlled delivery.
    pub(super) static NEXT_DELIVERY_GATE: RefCell<Option<Arc<DeliveryGate>>> = const { RefCell::new(None) };
    /// Completion gate retained by the delivery currently being executed.
    pub(super) static ACTIVE_DELIVERY_GATE: RefCell<Option<Arc<DeliveryGate>>> = const { RefCell::new(None) };
    /// Accept sends while dropping their receiver-side message callbacks.
    pub(super) static DROP_MESSAGES: Cell<bool> = const { Cell::new(false) };
    /// Seeded dummy delay and identifier state for deterministic simulations.
    pub(super) static CONTROLLED_RNG_STATE: Cell<Option<u64>> = const { Cell::new(None) };
    /// Virtual enqueue timestamp attached to this thread's controlled events.
    pub(super) static CONTROLLED_VIRTUAL_MS: Cell<u64> = const { Cell::new(0) };
}

/// Mutable controlled queue plus stable generation and sequence counters.
pub(super) struct ControlledDeliveryState {
    pub(super) queue: BTreeMap<u64, ControlledDeliveryEntry>,
    generation: u64,
    next_sequence: u64,
}

/// One queued callback with stable identity and virtual enqueue metadata.
pub(super) struct ControlledDeliveryEntry {
    pub(super) sequence: u64,
    pub(super) connection_id: String,
    pub(super) event: Event,
    pub(super) enqueued_virtual_ms: u64,
}

impl ControlledDeliveryState {
    pub(super) const fn new() -> Self {
        Self {
            queue: BTreeMap::new(),
            generation: 0,
            next_sequence: 0,
        }
    }

    pub(super) fn push_back(&mut self, entry: (String, Event)) {
        let (connection_id, event) = entry;
        let sequence = self.next_sequence;
        let enqueued_virtual_ms = CONTROLLED_VIRTUAL_MS.with(Cell::get);
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.queue.insert(sequence, ControlledDeliveryEntry {
            sequence,
            connection_id,
            event,
            enqueued_virtual_ms,
        });
        self.advance_generation();
    }

    pub(super) fn remove(&mut self, index: usize) -> Option<ControlledDeliveryEntry> {
        let sequence = self.queue.keys().nth(index).copied()?;
        self.remove_sequence(sequence)
    }

    pub(super) fn remove_sequence(&mut self, sequence: u64) -> Option<ControlledDeliveryEntry> {
        let entry = self.queue.remove(&sequence);
        if entry.is_some() {
            self.advance_generation();
        }
        entry
    }

    pub(super) fn clear(&mut self) {
        if !self.queue.is_empty() {
            self.queue.clear();
            self.advance_generation();
        }
    }

    pub(super) fn reset(&mut self) {
        self.queue.clear();
        self.next_sequence = 0;
        self.advance_generation();
    }

    pub(super) fn snapshot(&self) -> controlled::DeliverySnapshot {
        controlled::DeliverySnapshot::new(self.queue.len(), self.generation)
    }

    pub(super) fn inspect(&self, index: usize) -> Option<controlled::QueuedDelivery> {
        self.queue
            .values()
            .nth(index)
            .map(ControlledDeliveryEntry::inspect)
    }

    pub(super) fn inspect_after(&self, sequence: Option<u64>) -> Vec<controlled::QueuedDelivery> {
        let lower = sequence.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        self.queue
            .range((lower, std::ops::Bound::Unbounded))
            .map(|(_, entry)| entry.inspect())
            .collect()
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

impl ControlledDeliveryEntry {
    fn inspect(&self) -> controlled::QueuedDelivery {
        controlled::QueuedDelivery::new(
            self.sequence,
            self.connection_id.clone(),
            self.event.inspect(),
            self.enqueued_virtual_ms,
        )
    }
}
