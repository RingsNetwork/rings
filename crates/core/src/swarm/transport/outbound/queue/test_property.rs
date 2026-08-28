use std::collections::BTreeMap;
use std::collections::BTreeSet;

use super::*;

const MODEL_CAPACITY: usize = 8;
const ACTION_CARDINALITY: usize = 13;
const TRACE_LENGTH: u32 = 6;
const CLASSES: [TransferClass; TransferClass::COUNT] = [
    TransferClass::DhtControl,
    TransferClass::Storage,
    TransferClass::E2e,
    TransferClass::Application,
];
const LOWER_CLASSES: [TransferClass; 3] = [
    TransferClass::Storage,
    TransferClass::E2e,
    TransferClass::Application,
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ModelTransfer {
    id: u16,
    next_frame: u8,
    frames: u8,
}

struct SchedulerHarness {
    actual: TransferQueues<ModelTransfer>,
    pending_deliveries: BTreeMap<u64, TransferClass>,
    live: BTreeSet<u16>,
    terminated: BTreeSet<u16>,
    admitted: Vec<(TransferClass, u16, u8)>,
    next_transfer_id: u16,
    next_delivery_id: u64,
    shutdown: bool,
}

impl SchedulerHarness {
    fn new() -> Self {
        Self {
            actual: TransferQueues::default(),
            pending_deliveries: BTreeMap::new(),
            live: BTreeSet::new(),
            terminated: BTreeSet::new(),
            admitted: Vec::new(),
            next_transfer_id: 0,
            next_delivery_id: 0,
            shutdown: false,
        }
    }

    fn apply(&mut self, action: usize) {
        if self.shutdown {
            return;
        }
        match action {
            0..=3 => self.submit(CLASSES[action]),
            4 => {
                let _ = self.admit_one_frame();
            }
            5 => self.complete_delivery(false),
            6 => self.complete_delivery(true),
            7 => self.cancel_ready(|id| id.is_multiple_of(2)),
            8 => self.cancel_ready(|id| !id.is_multiple_of(2)),
            9 => self.fail_admission(),
            10 => self.terminate_delivery(false),
            11 => self.terminate_delivery(true),
            12 => self.shutdown(),
            _ => {}
        }
        assert!(self.live.len() <= MODEL_CAPACITY);
        self.assert_fifo_and_frame_contiguity();
    }

    fn submit(&mut self, class: TransferClass) {
        if self.live.len() == MODEL_CAPACITY {
            return;
        }
        let transfer = ModelTransfer {
            id: self.next_transfer_id,
            next_frame: 0,
            frames: 1 + u8::from(self.next_transfer_id.is_multiple_of(2)),
        };
        self.next_transfer_id = self.next_transfer_id.saturating_add(1);
        assert!(self.live.insert(transfer.id));
        self.actual.push(class, transfer);
    }

    fn pop_attempt(&mut self) -> Option<RunnableTransfer<ModelTransfer>> {
        loop {
            let actual = self.actual.pop()?;
            let item = *actual.item();
            assert!(self.live.contains(&item.id));
            assert!(!self.terminated.contains(&item.id));
            if item.next_frame == item.frames {
                let completed = self.actual.finish_transfer(actual);
                self.terminate(completed.id);
                continue;
            }
            return Some(actual);
        }
    }

    fn admit_one_frame(&mut self) -> Option<TransferClass> {
        let mut actual = self.pop_attempt()?;
        let class = actual.class();
        let id = actual.item().id;
        let frame = actual.item().next_frame;
        assert!(!self.terminated.contains(&id));
        self.admitted.push((class, id, frame));
        actual.item_mut().next_frame = frame.saturating_add(1);
        self.actual.record_frame_admitted(class);

        let delivery_id = self.next_delivery_id;
        self.next_delivery_id = self.next_delivery_id.saturating_add(1);
        self.actual.wait_for_delivery(delivery_id, actual);
        assert!(self.pending_deliveries.insert(delivery_id, class).is_none());
        Some(class)
    }

    fn selected_delivery(&self, newest: bool) -> Option<(u64, TransferClass)> {
        if newest {
            self.pending_deliveries.last_key_value()
        } else {
            self.pending_deliveries.first_key_value()
        }
        .map(|(id, class)| (*id, *class))
    }

    fn complete_delivery(&mut self, newest: bool) {
        let Some((delivery_id, class)) = self.selected_delivery(newest) else {
            return;
        };
        let actual = self
            .actual
            .take_waiting(class, delivery_id)
            .expect("delivery id must identify its waiting class");
        self.actual.make_runnable(actual);
        self.pending_deliveries.remove(&delivery_id);
    }

    fn fail_admission(&mut self) {
        let Some(actual) = self.pop_attempt() else {
            return;
        };
        let failed = self.actual.fail_attempt(actual);
        self.terminate(failed.id);
    }

    fn terminate_delivery(&mut self, newest: bool) {
        let Some((delivery_id, class)) = self.selected_delivery(newest) else {
            return;
        };
        let actual = self
            .actual
            .take_waiting(class, delivery_id)
            .expect("delivery id must identify its waiting class");
        let terminated = self.actual.finish_transfer(actual);
        self.pending_deliveries.remove(&delivery_id);
        self.terminate(terminated.id);
    }

    fn cancel_ready(&mut self, predicate: impl Fn(u16) -> bool) {
        let cancelled = self
            .actual
            .remove_ready_where(|transfer| predicate(transfer.id));
        for transfer in cancelled {
            self.terminate(transfer.id);
        }
    }

    fn terminate(&mut self, id: u16) {
        assert!(self.live.remove(&id), "transfer {id} terminated twice");
        assert!(self.terminated.insert(id), "transfer {id} terminated twice");
    }

    fn shutdown(&mut self) {
        let drained = self.actual.drain_transfers();
        let drained_ids = drained
            .iter()
            .map(|transfer| transfer.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(drained_ids, self.live);
        assert_eq!(drained_ids.len(), drained.len());
        for transfer in drained {
            self.terminate(transfer.id);
        }
        self.pending_deliveries.clear();
        self.shutdown = true;
    }

    fn assert_fifo_and_frame_contiguity(&self) {
        let mut last = [None; TransferClass::COUNT];
        for &(class, id, frame) in &self.admitted {
            if let Some((last_id, last_frame)) = last[class.index()] {
                if id == last_id {
                    assert_eq!(frame, last_frame + 1);
                } else {
                    assert!(id > last_id, "same-class FIFO order regressed");
                    assert_eq!(frame, 0, "a successor started after its first frame");
                }
            } else {
                assert_eq!(frame, 0);
            }
            last[class.index()] = Some((id, frame));
        }
    }
}

fn admit_single_frame(queue: &mut TransferQueues<u16>) -> (TransferClass, u16) {
    let selected = queue
        .pop()
        .expect("continuous fixture must remain runnable");
    let class = selected.class();
    queue.record_frame_admitted(class);
    let id = queue.finish_transfer(selected);
    (class, id)
}

#[test]
fn test_all_short_traces_preserve_fifo_cancellation_and_shutdown_invariants() {
    let trace_count = ACTION_CARDINALITY.pow(TRACE_LENGTH);
    for encoded in 0..trace_count {
        let mut code = encoded;
        let mut harness = SchedulerHarness::new();
        for _ in 0..TRACE_LENGTH {
            harness.apply(code % ACTION_CARDINALITY);
            code /= ACTION_CARDINALITY;
        }
        harness.shutdown();
        assert!(harness.live.is_empty());
        assert!(harness.pending_deliveries.is_empty());
    }
}

#[test]
fn test_sustained_mixed_load_bounds_control_and_rotates_lower_service() {
    let mut queue = TransferQueues::default();
    for id in 0..48 {
        queue.push(TransferClass::DhtControl, id);
    }
    for (offset, class) in LOWER_CLASSES.into_iter().enumerate() {
        for sequence in 0..8 {
            queue.push(class, 100 + (offset as u16 * 10) + sequence);
        }
    }

    let selected = (0..24)
        .map(|_| admit_single_frame(&mut queue).0)
        .collect::<Vec<_>>();
    assert!(selected
        .windows(OUTBOUND_CONTROL_BURST + 1)
        .all(|window| window
            .iter()
            .any(|class| *class != TransferClass::DhtControl)));
    let lower = selected
        .into_iter()
        .filter(|class| *class != TransferClass::DhtControl)
        .collect::<Vec<_>>();
    for (index, class) in lower.into_iter().enumerate() {
        assert_eq!(class, LOWER_CLASSES[index % LOWER_CLASSES.len()]);
    }
}

#[test]
fn test_fourth_control_frame_is_followed_by_waiting_lower_work() {
    let mut queue = TransferQueues::default();
    for id in 0..5 {
        queue.push(TransferClass::DhtControl, id);
    }
    queue.push(TransferClass::Storage, 100);

    let selected = (0..=OUTBOUND_CONTROL_BURST)
        .map(|_| admit_single_frame(&mut queue).0)
        .collect::<Vec<_>>();
    assert_eq!(selected, vec![
        TransferClass::DhtControl,
        TransferClass::DhtControl,
        TransferClass::DhtControl,
        TransferClass::DhtControl,
        TransferClass::Storage,
    ]);
}

#[cfg(all(feature = "dummy", not(target_family = "wasm")))]
#[test]
fn test_bounded_control_burst_ablation_changes_the_real_queue_policy() {
    let _runtime = crate::simulation::SimulationRuntimeGuard::enter(
        41,
        1_700_000_000_000,
        crate::simulation::ProtectionProfile::without_bounded_control_burst(),
    )
    .expect("simulation runtime must install");
    let mut queue = TransferQueues::default();
    for id in 0..5 {
        queue.push(TransferClass::DhtControl, id);
    }
    queue.push(TransferClass::Storage, 100);

    let selected = (0..=OUTBOUND_CONTROL_BURST)
        .map(|_| admit_single_frame(&mut queue).0)
        .collect::<Vec<_>>();
    assert_eq!(selected, vec![TransferClass::DhtControl; 5]);
}
