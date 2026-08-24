use std::collections::VecDeque;

use super::model::TransferClass;

/// Four control frames followed by one lower-class frame gives control at most
/// 80% of frame admissions under sustained mixed load.
pub(super) const OUTBOUND_CONTROL_BURST: usize = 4;
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

enum TransferLaneState<T> {
    Idle,
    Runnable(T),
    WaitingDelivery { id: u64, item: T },
}

struct TransferLane<T> {
    state: TransferLaneState<T>,
    queued: VecDeque<T>,
}

impl<T> Default for TransferLane<T> {
    fn default() -> Self {
        Self {
            state: TransferLaneState::Idle,
            queued: VecDeque::new(),
        }
    }
}

impl<T> TransferLane<T> {
    fn enqueue(&mut self, item: T) {
        if matches!(self.state, TransferLaneState::Idle) {
            self.state = TransferLaneState::Runnable(item);
        } else {
            self.queued.push_back(item);
        }
    }

    fn is_runnable(&self) -> bool {
        matches!(self.state, TransferLaneState::Runnable(_))
    }

    fn take_runnable(&mut self) -> Option<T> {
        match std::mem::replace(&mut self.state, TransferLaneState::Idle) {
            TransferLaneState::Runnable(item) => Some(item),
            state => {
                self.state = state;
                None
            }
        }
    }

    fn wait_for_delivery(&mut self, id: u64, item: T) {
        debug_assert!(matches!(self.state, TransferLaneState::Idle));
        self.state = TransferLaneState::WaitingDelivery { id, item };
    }

    fn take_waiting(&mut self, id: u64) -> Option<T> {
        match std::mem::replace(&mut self.state, TransferLaneState::Idle) {
            TransferLaneState::WaitingDelivery {
                id: waiting_id,
                item,
            } if waiting_id == id => Some(item),
            state => {
                self.state = state;
                None
            }
        }
    }

    fn make_runnable(&mut self, item: T) {
        debug_assert!(matches!(self.state, TransferLaneState::Idle));
        self.state = TransferLaneState::Runnable(item);
    }

    fn finish_current(&mut self) {
        self.state = self
            .queued
            .pop_front()
            .map_or(TransferLaneState::Idle, TransferLaneState::Runnable);
    }

    fn drain_transfers(&mut self) -> Vec<T> {
        let mut transfers = Vec::with_capacity(self.queued.len().saturating_add(1));
        match std::mem::replace(&mut self.state, TransferLaneState::Idle) {
            TransferLaneState::Runnable(item) | TransferLaneState::WaitingDelivery { item, .. } => {
                transfers.push(item)
            }
            TransferLaneState::Idle => {}
        }
        transfers.extend(self.queued.drain(..));
        transfers
    }
}

/// Runnable lane head whose class was selected by this queue.
#[must_use]
pub(super) struct RunnableTransfer<T> {
    class: TransferClass,
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

    #[cfg(test)]
    pub(super) fn into_item(self) -> T {
        self.item
    }
}

pub(super) struct TransferQueues<T> {
    lanes: [TransferLane<T>; TransferClass::COUNT],
    lower_cursor: usize,
    consecutive_control: usize,
}

impl<T> Default for TransferQueues<T> {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| TransferLane::default()),
            lower_cursor: 0,
            consecutive_control: 0,
        }
    }
}

impl<T> TransferQueues<T> {
    pub(super) fn push(&mut self, class: TransferClass, item: T) {
        self.lane_mut(class).enqueue(item);
    }

    pub(super) fn pop(&mut self) -> Option<RunnableTransfer<T>> {
        let has_control = self.is_runnable(TransferClass::DhtControl);
        let selected = if has_control
            && (self.consecutive_control < OUTBOUND_CONTROL_BURST || !self.has_lower())
        {
            Some(TransferClass::DhtControl)
        } else {
            self.next_lower_class()
        }?;
        self.take(selected).map(|item| RunnableTransfer {
            class: selected,
            item,
        })
    }

    pub(super) fn wait_for_delivery(&mut self, id: u64, transfer: RunnableTransfer<T>) {
        self.lane_mut(transfer.class)
            .wait_for_delivery(id, transfer.item);
    }

    pub(super) fn take_waiting(
        &mut self,
        class: TransferClass,
        id: u64,
    ) -> Option<RunnableTransfer<T>> {
        self.lane_mut(class)
            .take_waiting(id)
            .map(|item| RunnableTransfer { class, item })
    }

    pub(super) fn make_runnable(&mut self, transfer: RunnableTransfer<T>) {
        self.lane_mut(transfer.class).make_runnable(transfer.item);
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

    pub(super) fn finish_current(&mut self, class: TransferClass) {
        self.lane_mut(class).finish_current();
    }

    pub(super) fn finish_transfer(&mut self, transfer: RunnableTransfer<T>) -> T {
        let (class, item) = transfer.into_parts();
        self.finish_current(class);
        item
    }

    pub(super) fn fail_attempt(&mut self, transfer: RunnableTransfer<T>) -> T {
        let (class, item) = transfer.into_parts();
        self.advance_fairness(class);
        self.finish_current(class);
        item
    }

    pub(super) fn drain_transfers(&mut self) -> Vec<T> {
        self.lanes
            .iter_mut()
            .flat_map(TransferLane::drain_transfers)
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

    fn take(&mut self, class: TransferClass) -> Option<T> {
        self.lane_mut(class).take_runnable()
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
