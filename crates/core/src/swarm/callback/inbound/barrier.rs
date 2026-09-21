//! Ordering barrier between completed reassembly and logical-data lanes.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::InboundLane;

pub(super) struct ReassemblyHandoffBarrier {
    pub(super) sequence: u64,
    started: Arc<AtomicBool>,
}

impl ReassemblyHandoffBarrier {
    pub(super) fn new(sequence: u64) -> Self {
        Self {
            sequence,
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn start_marker(&self) -> Arc<AtomicBool> {
        self.started.clone()
    }

    pub(super) fn has_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// The reassembly lane itself waits for the hand-off to start; logical
    /// data lanes are ordered against it by the actor's reassembly barrier
    /// sequence, which already includes this barrier.
    pub(super) fn blocks(&self, lane: InboundLane) -> bool {
        lane == InboundLane::Reassembly
    }
}

pub(super) fn lane_waits_for_reassembly(lane: InboundLane) -> bool {
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    if !crate::simulation::protection_profile().barrier_control_exemption() {
        return true;
    }
    lane.is_logical_data()
}
