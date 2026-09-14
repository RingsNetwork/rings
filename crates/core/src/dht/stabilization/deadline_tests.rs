//! Deadline tests for stabilization sub-step cancellation.
//!
//! These tests exercise the ownership consequence of the deadline race: once
//! the timer wins, the unfinished stabilization future must be dropped rather
//! than continuing detached from the serial maintenance loop.

use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::*;

/// Records whether ownership of a pending test future was released.
///
/// The witness has no behavior before destruction; its `Drop` implementation is
/// the observable proof that the losing future from `await_step_deadline` was
/// cancelled.
struct DropWitness(
    /// Shared flag written exactly when the witness leaves the pending future.
    Arc<AtomicBool>,
);

impl Drop for DropWitness {
    /// Publish cancellation to the test thread when the pending future is dropped.
    ///
    /// Release ordering pairs with the test's acquire load, making destruction
    /// of the losing future visible before the final assertion.
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_family = "wasm"), tokio::test)]
/// Proves that a timed-out stabilization future is cancelled, not detached.
///
/// The work future can never complete on its own. Therefore the timeout result
/// and the drop witness together establish that the select race returned the
/// deadline outcome and destroyed the still-pending loser.
async fn test_step_deadline_drops_work_that_does_not_complete() {
    let dropped = Arc::new(AtomicBool::new(false));
    let witness = dropped.clone();
    // Pending forever exercises the timeout branch; `DropWitness` proves the
    // loser future is actually dropped by the select race.
    let future = async move {
        let _witness = DropWitness(witness);
        futures::future::pending::<()>().await;
        Ok(())
    };

    let result = await_step_deadline(future, Duration::from_millis(1)).await;

    assert!(matches!(result, StepDeadline::TimedOut));
    assert!(dropped.load(Ordering::Acquire));
}
