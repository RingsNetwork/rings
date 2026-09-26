//! Controlled in-memory network for processor tests (`dummy` builds only).
//!
//! Under the `dummy` feature every processor uses the dummy transport. Its default mode sleeps
//! a random 10–100 ms per message, and only its controlled mode is deterministic: every event
//! waits in a thread-local FIFO queue until a test delivers it. [`ControlledNetwork`] enables
//! that mode for one test and runs a pump that delivers the oldest queued event whenever one
//! is waiting, so the test's own awaits (admission, inbound messages, lookups) progress without
//! any message waiting on a clock; the helpers' waits are activity-woken probes whose only
//! timer is a hang guard:
//!
//! ```text
//! pump:  loop { ¬paused ∧ pending() > 0 ? deliver(oldest) : () ; yield }
//! ```
//!
//! The queue is thread-local and the tests run on a current-thread runtime, so the pump, the
//! processors' spawned tasks and the test all share the one queue. A test that needs a
//! specific delivery order pauses the pump and delivers explicitly.
//!
//! The dummy connection ids are seeded per test. A test whose identities are also fixed (the
//! E2E stream test) has a reproducible schedule; tests that draw random identities have a
//! clock-free but identity-dependent schedule.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use rings_transport::connections::dummy_controlled;

use crate::tests::activity::record_activity;

/// Base seed of the dummy connection identifiers; see [`test_seed`].
const CONTROLLED_NETWORK_SEED: u64 = 883;

/// The dummy seed of the running test: [`CONTROLLED_NETWORK_SEED`] mixed with an FNV-1a hash of
/// the test's name (libtest names each test's thread after it; an unnamed thread fails the
/// test instead of falling back to a seed shared with other tests).
///
/// Ids are then stable for one test and distinct across tests, so no two tests register the
/// same id in the dummy transport's process-wide connection registry.
fn test_seed() -> u64 {
    let name_hash = std::thread::current()
        .name()
        .expect("libtest names each test's thread after the test")
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    dummy_controlled::mix_seed(CONTROLLED_NETWORK_SEED ^ name_hash)
}

/// Upper bound on explicit delivery steps in [`ControlledNetwork::deliver_newest_until`]. A
/// step is one observation, at most one delivery and one cooperative yield, so the bound counts
/// events, never wall-clock time.
const CONTROLLED_STEP_BOUND: usize = 16_384;

/// Controlled delivery for the lifetime of one test, driven by a FIFO pump.
pub(super) struct ControlledNetwork {
    /// Whether the pump is suspended so the test can choose the delivery order.
    paused: Arc<AtomicBool>,
    /// The pump task, aborted when the network is dropped.
    pump: tokio::task::JoinHandle<()>,
}

impl ControlledNetwork {
    /// Enable controlled delivery with this test's seed and start the FIFO pump.
    ///
    /// Pre: called on the test's current-thread runtime, before any connection is made.
    pub(super) fn start() -> Self {
        dummy_controlled::enable(true);
        dummy_controlled::set_seed(test_seed());
        let paused = Arc::new(AtomicBool::new(false));
        let pump_paused = Arc::clone(&paused);
        let pump = tokio::spawn(async move {
            loop {
                if !pump_paused.load(Ordering::Acquire) && dummy_controlled::pending() > 0 {
                    // A target retired meanwhile is a legal outcome; the event is consumed.
                    dummy_controlled::deliver(0).await;
                    // A delivered event is a state change that tests probe for.
                    record_activity();
                }
                tokio::task::yield_now().await;
            }
        });
        Self { paused, pump }
    }

    /// Suspend the pump; queued and newly sent events wait until delivered explicitly.
    pub(super) fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    /// Resume FIFO delivery by the pump.
    pub(super) fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }

    /// With the pump paused, deliver the **newest** queued event first until `reached` holds.
    ///
    /// Delivering newest-first reverses the order in which events were sent, which exercises a
    /// receiver's order-insensitivity deterministically.
    ///
    /// Post: returns only after `reached` was observed true; otherwise panics with `label`
    /// after [`CONTROLLED_STEP_BOUND`] steps.
    pub(super) async fn deliver_newest_until(
        &self,
        label: &str,
        mut reached: impl FnMut() -> bool,
    ) {
        for _ in 0..CONTROLLED_STEP_BOUND {
            if reached() {
                return;
            }
            let pending = dummy_controlled::pending();
            if pending > 0 {
                dummy_controlled::deliver(pending - 1).await;
                record_activity();
            }
            tokio::task::yield_now().await;
        }
        panic!("{label} not reached within {CONTROLLED_STEP_BOUND} controlled steps");
    }
}

impl Drop for ControlledNetwork {
    fn drop(&mut self) {
        self.pump.abort();
        dummy_controlled::enable(false);
    }
}
