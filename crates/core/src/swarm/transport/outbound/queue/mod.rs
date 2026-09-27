use std::collections::VecDeque;

use super::model::TransferClass;

/// Four control frames followed by one lower-class frame gives control at most
/// 80% of frame admissions under sustained mixed load.
pub(crate) const OUTBOUND_CONTROL_BURST: usize = 4;
const LOWER_CLASSES: [TransferClass; 3] = [
    TransferClass::Storage,
    TransferClass::E2e,
    TransferClass::Application,
];

macro_rules! lane_for_class {
    ($lanes:expr, $class:expr) => {{
        let [control, storage, e2e, application] = $lanes;
        match $class {
            TransferClass::DhtControl => control,
            TransferClass::Storage => storage,
            TransferClass::E2e => e2e,
            TransferClass::Application => application,
        }
    }};
}

/// Transfers of one class lane that may be in flight at once (#899).
///
/// A lane no longer waits for each delivery before its next transfer: up to this many
/// transfers hold a window slot, so consecutive frames reach the peer back to back and its SCTP
/// SACKs pair up instead of waiting out the delayed-ACK timer. It stays strictly below
/// `TRANSACTION_REPLAY_WINDOW`, so pipelining inside one lane can never reorder a class's
/// transactions beyond the receiver's replay window.
pub(crate) const OUTBOUND_LANE_WINDOW: usize = 8;

const _: () = assert!(OUTBOUND_LANE_WINDOW > 0);
const _: () = assert!(OUTBOUND_LANE_WINDOW < crate::message::TRANSACTION_REPLAY_WINDOW);

/// Whether an admitted frame was its transfer's last.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameRemainder {
    /// The transfer has admitted every frame; only its last delivery is outstanding.
    Final,
    /// The transfer still has frames to admit.
    More,
}

/// State of one transfer that holds a window slot.
enum SlotState<T> {
    /// Ready for its next frame; `fresh` until its first frame is admitted.
    Runnable {
        /// The transfer.
        item: T,
        /// Whether no frame of the transfer has been admitted yet.
        fresh: bool,
    },
    /// Taken by the worker, which is admitting one of its frames.
    Sending,
    /// One frame admitted; the transfer waits for that frame's delivery `delivery`.
    Waiting {
        /// Identity of the delivery future the transfer waits on.
        delivery: u64,
        /// Whether frames remain after the one in flight.
        remainder: FrameRemainder,
        /// The waiting transfer.
        item: T,
    },
}

impl<T> SlotState<T> {
    /// Whether this transfer waits on the delivery `delivery`.
    fn waits_on(&self, delivery: u64) -> bool {
        matches!(self, Self::Waiting { delivery: waiting, .. } if *waiting == delivery)
    }

    /// Whether this transfer has frames it has not admitted yet, while one is in flight: a
    /// fresh transfer behind it must not start, so its frames stay contiguous on the wire.
    fn holds_the_wire(&self) -> bool {
        matches!(self, Self::Waiting {
            remainder: FrameRemainder::More,
            ..
        })
    }
}

/// One class lane: a window of at most `window` in-flight transfers, oldest first, and the
/// FIFO backlog behind it.
///
/// Laws, over every sequence of operations:
/// - **Bound.** `slots.len() ≤ window`: at most `window` transfers are in flight.
/// - **FIFO.** Transfers enter the window in push order, and `take_runnable` always takes the
///   oldest runnable slot, so first frames are admitted in push order and each transfer's
///   frames stay in order.
/// - **Contiguity.** A fresh transfer is not started while an earlier one still has frames to
///   admit, so the frames of one lane's chunked transfers never interleave on the wire. Only
///   transfers whose last frame is in flight overlap: single-frame transfers pipeline freely.
/// - **Attribution.** A slot is addressed only by its own token or its own delivery identity,
///   so every outcome settles the transfer it belongs to.
struct TransferLane<T> {
    /// In-flight transfers in admission order, each with its lane-local slot token.
    slots: VecDeque<(u64, SlotState<T>)>,
    /// Transfers waiting for a window slot, in push order.
    queued: VecDeque<T>,
    /// The next slot token to issue.
    next_slot: u64,
}

impl<T> Default for TransferLane<T> {
    fn default() -> Self {
        Self {
            slots: VecDeque::new(),
            queued: VecDeque::new(),
            next_slot: 0,
        }
    }
}

impl<T> TransferLane<T> {
    /// Give `item` a window slot if one is free, else queue it behind the others.
    fn enqueue(&mut self, item: T, window: usize) {
        self.queued.push_back(item);
        self.refill(window);
    }

    /// Move queued transfers into free window slots, oldest first.
    fn refill(&mut self, window: usize) {
        while self.slots.len() < window {
            let Some(item) = self.queued.pop_front() else {
                return;
            };
            let slot = self.next_slot;
            self.next_slot = self.next_slot.wrapping_add(1);
            self.slots
                .push_back((slot, SlotState::Runnable { item, fresh: true }));
        }
    }

    /// Position of the oldest slot that may admit a frame now: a runnable slot, unless it is
    /// fresh and an earlier transfer still holds the wire.
    fn runnable_position(&self) -> Option<usize> {
        let mut wire_held = false;
        for (position, (_, state)) in self.slots.iter().enumerate() {
            match state {
                SlotState::Runnable { fresh: false, .. } => return Some(position),
                SlotState::Runnable { fresh: true, .. } if !wire_held => return Some(position),
                state => wire_held |= state.holds_the_wire(),
            }
        }
        None
    }

    /// Whether some in-flight transfer may admit a frame now.
    fn is_runnable(&self) -> bool {
        self.runnable_position().is_some()
    }

    /// Take the transfer at [`Self::runnable_position`], leaving its slot `Sending`.
    fn take_runnable(&mut self) -> Option<(u64, T)> {
        let position = self.runnable_position()?;
        let (slot, state) = self.slots.get_mut(position)?;
        match std::mem::replace(state, SlotState::Sending) {
            SlotState::Runnable { item, .. } => Some((*slot, item)),
            other => {
                *state = other;
                None
            }
        }
    }

    /// The state of the slot `slot`, if it is still in flight.
    fn state_mut(&mut self, slot: u64) -> Option<&mut SlotState<T>> {
        self.slots
            .iter_mut()
            .find(|(token, _)| *token == slot)
            .map(|(_, state)| state)
    }

    /// Park a sent transfer on its frame's delivery `delivery`.
    fn wait_for_delivery(&mut self, slot: u64, delivery: u64, remainder: FrameRemainder, item: T) {
        match self.state_mut(slot) {
            Some(state @ SlotState::Sending) => {
                *state = SlotState::Waiting {
                    delivery,
                    remainder,
                    item,
                }
            }
            _ => debug_assert!(false, "only a sending slot can wait for delivery"),
        }
    }

    /// Take the transfer waiting on `delivery`, leaving its slot `Sending`.
    fn take_waiting(&mut self, delivery: u64) -> Option<(u64, T)> {
        let (slot, state) = self
            .slots
            .iter_mut()
            .find(|(_, state)| state.waits_on(delivery))?;
        match std::mem::replace(state, SlotState::Sending) {
            SlotState::Waiting { item, .. } => Some((*slot, item)),
            other => {
                *state = other;
                None
            }
        }
    }

    /// Return a sending transfer to its slot, ready for its next frame.
    fn make_runnable(&mut self, slot: u64, item: T) {
        match self.state_mut(slot) {
            Some(state @ SlotState::Sending) => *state = SlotState::Runnable { item, fresh: false },
            _ => debug_assert!(false, "only a sending slot can become runnable again"),
        }
    }

    /// Release the slot of a finished transfer and admit the next queued one.
    fn finish(&mut self, slot: u64, window: usize) {
        self.slots.retain(|(token, _)| *token != slot);
        self.refill(window);
    }

    /// Every transfer the lane still owns: runnable and waiting slots, then the backlog.
    /// A `Sending` slot's transfer is owned by the worker.
    fn drain_transfers(&mut self) -> Vec<T> {
        let mut transfers = Vec::with_capacity(self.slots.len().saturating_add(self.queued.len()));
        for (_, state) in self.slots.drain(..) {
            match state {
                SlotState::Runnable { item, .. } | SlotState::Waiting { item, .. } => {
                    transfers.push(item)
                }
                SlotState::Sending => {}
            }
        }
        transfers.extend(self.queued.drain(..));
        transfers
    }

    /// Remove queued and runnable transfers matching `predicate`, leaving every transfer
    /// whose delivery future is still active, then refill the freed slots.
    fn remove_ready_where(
        &mut self,
        predicate: &mut impl FnMut(&T) -> bool,
        window: usize,
    ) -> Vec<T> {
        let mut removed = Vec::new();
        let mut retained = VecDeque::with_capacity(self.queued.len());
        while let Some(item) = self.queued.pop_front() {
            if predicate(&item) {
                removed.push(item);
            } else {
                retained.push_back(item);
            }
        }
        self.queued = retained;

        let mut slots = VecDeque::with_capacity(self.slots.len());
        for (slot, state) in self.slots.drain(..) {
            match state {
                SlotState::Runnable { item, .. } if predicate(&item) => removed.push(item),
                state => slots.push_back((slot, state)),
            }
        }
        self.slots = slots;
        self.refill(window);
        removed
    }

    /// Transfers holding a window slot.
    #[cfg(test)]
    fn in_flight(&self) -> usize {
        self.slots.len()
    }
}

/// A transfer taken from its lane's window by this queue, with the slot it holds.
#[must_use]
pub(super) struct RunnableTransfer<T> {
    /// The lane the transfer belongs to.
    class: TransferClass,
    /// The window slot the transfer holds while the worker owns it.
    slot: u64,
    /// The transfer.
    item: T,
}

impl<T> RunnableTransfer<T> {
    pub(super) const fn class(&self) -> TransferClass {
        self.class
    }

    pub(super) fn item(&self) -> &T {
        &self.item
    }

    pub(super) fn item_mut(&mut self) -> &mut T {
        &mut self.item
    }

    pub(super) fn into_parts(self) -> (TransferClass, T) {
        (self.class, self.item)
    }
}

pub(super) struct TransferQueues<T> {
    lanes: [TransferLane<T>; TransferClass::COUNT],
    lower_cursor: usize,
    consecutive_control: usize,
    /// In-flight transfers allowed per lane.
    window: usize,
}

impl<T> Default for TransferQueues<T> {
    fn default() -> Self {
        Self::with_window(OUTBOUND_LANE_WINDOW)
    }
}

impl<T> TransferQueues<T> {
    /// Queues whose lanes each keep at most `window` transfers in flight.
    fn with_window(window: usize) -> Self {
        Self {
            lanes: std::array::from_fn(|_| TransferLane::default()),
            lower_cursor: 0,
            consecutive_control: 0,
            window,
        }
    }

    /// Queues with a small window, so that tests reach the bound.
    #[cfg(test)]
    pub(super) fn with_window_for_test(window: usize) -> Self {
        Self::with_window(window)
    }

    /// Transfers of `class` holding a window slot.
    #[cfg(test)]
    pub(super) fn in_flight(&self, class: TransferClass) -> usize {
        self.lane(class).in_flight()
    }

    pub(super) fn push(&mut self, class: TransferClass, item: T) {
        let window = self.window;
        self.lane_mut(class).enqueue(item, window);
    }

    pub(super) fn pop(&mut self) -> Option<RunnableTransfer<T>> {
        let has_control = self.is_runnable(TransferClass::DhtControl);
        let has_lower = self.has_lower();
        let selected = if has_control
            && (!bounded_control_burst_enabled()
                || self.consecutive_control < OUTBOUND_CONTROL_BURST
                || !has_lower)
        {
            Some(TransferClass::DhtControl)
        } else {
            self.next_lower_class()
        }?;
        #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
        if selected == TransferClass::DhtControl
            && has_lower
            && self.consecutive_control >= OUTBOUND_CONTROL_BURST
        {
            crate::simulation::record_protection_violation(
                crate::simulation::ProtectionLayer::BoundedControlBurst,
            );
        }
        self.lane_mut(selected)
            .take_runnable()
            .map(|(slot, item)| RunnableTransfer {
                class: selected,
                slot,
                item,
            })
    }

    /// Park a transfer whose frame was admitted on that frame's delivery `id`. Its slot stays
    /// taken; once its last frame is in flight (`remainder` is `Final`) the lane may start the
    /// next transfers meanwhile.
    pub(super) fn wait_for_delivery(
        &mut self,
        id: u64,
        remainder: FrameRemainder,
        transfer: RunnableTransfer<T>,
    ) {
        self.lane_mut(transfer.class).wait_for_delivery(
            transfer.slot,
            id,
            remainder,
            transfer.item,
        );
    }

    /// Take the transfer of `class` waiting on delivery `id`.
    pub(super) fn take_waiting(
        &mut self,
        class: TransferClass,
        id: u64,
    ) -> Option<RunnableTransfer<T>> {
        self.lane_mut(class)
            .take_waiting(id)
            .map(|(slot, item)| RunnableTransfer { class, slot, item })
    }

    /// Return a delivered transfer to its slot, ready for its next frame.
    pub(super) fn make_runnable(&mut self, transfer: RunnableTransfer<T>) {
        self.lane_mut(transfer.class)
            .make_runnable(transfer.slot, transfer.item);
    }

    pub(super) fn record_frame_admitted(&mut self, class: TransferClass) {
        self.advance_fairness(class);
    }

    fn advance_fairness(&mut self, class: TransferClass) {
        if class == TransferClass::DhtControl {
            self.consecutive_control = self.consecutive_control.saturating_add(1);
            return;
        }
        self.consecutive_control = 0;
        if let Some(index) = LOWER_CLASSES
            .iter()
            .position(|candidate| *candidate == class)
        {
            self.lower_cursor = index.saturating_add(1) % LOWER_CLASSES.len();
        }
    }

    /// Release a finished transfer's slot to the next queued transfer of its lane.
    pub(super) fn finish_transfer(&mut self, transfer: RunnableTransfer<T>) -> T {
        let RunnableTransfer { class, slot, item } = transfer;
        let window = self.window;
        self.lane_mut(class).finish(slot, window);
        item
    }

    /// Release a failed transfer's slot, charging its lane one scheduling turn.
    pub(super) fn fail_attempt(&mut self, transfer: RunnableTransfer<T>) -> T {
        self.advance_fairness(transfer.class);
        self.finish_transfer(transfer)
    }

    pub(super) fn drain_transfers(&mut self) -> Vec<T> {
        self.lanes
            .iter_mut()
            .flat_map(TransferLane::drain_transfers)
            .collect()
    }

    /// Remove runnable and queued items matching `predicate` without disturbing
    /// any transfer whose delivery future is still active.
    pub(super) fn remove_ready_where(&mut self, mut predicate: impl FnMut(&T) -> bool) -> Vec<T> {
        let window = self.window;
        self.lanes
            .iter_mut()
            .flat_map(|lane| lane.remove_ready_where(&mut predicate, window))
            .collect()
    }

    fn is_runnable(&self, class: TransferClass) -> bool {
        self.lane(class).is_runnable()
    }

    fn has_lower(&self) -> bool {
        LOWER_CLASSES
            .iter()
            .copied()
            .any(|class| self.is_runnable(class))
    }

    fn lane(&self, class: TransferClass) -> &TransferLane<T> {
        lane_for_class!(&self.lanes, class)
    }

    fn lane_mut(&mut self, class: TransferClass) -> &mut TransferLane<T> {
        lane_for_class!(&mut self.lanes, class)
    }

    fn next_lower_class(&self) -> Option<TransferClass> {
        LOWER_CLASSES
            .iter()
            .copied()
            .cycle()
            .skip(self.lower_cursor)
            .take(LOWER_CLASSES.len())
            .find(|class| self.is_runnable(*class))
    }
}

fn bounded_control_burst_enabled() -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    {
        crate::simulation::protection_profile().bounded_control_burst()
    }
    #[cfg(not(all(test, feature = "dummy", not(target_family = "wasm"))))]
    {
        true
    }
}

#[cfg(test)]
mod test_property;
