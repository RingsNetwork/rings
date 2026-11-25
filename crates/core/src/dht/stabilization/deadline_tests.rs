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

#[cfg(not(target_family = "wasm"))]
#[tracing_test::traced_test]
#[tokio::test]
async fn test_run_step_logs_transient_routing_refusals_at_debug_and_other_errors_at_error(
) -> Result<()> {
    use crate::delegation::DelegateeKey;
    use crate::ecc::SecretKey;
    use crate::storage::MemStorage;
    use crate::swarm::SwarmBuilder;

    let session = DelegateeKey::new_with_seckey(&SecretKey::random())?;
    let swarm = Arc::new(
        SwarmBuilder::new(
            0,
            crate::tests::test_transport(""),
            Box::new(MemStorage::new()),
            session,
        )
        .replay_storage(crate::tests::volatile_replay_storage())
        .build(),
    );
    let stabilizer = swarm.stabilizer();
    let peer = Did::from(7_u32);

    assert!(stabilizer
        .run_step(
            "transient_not_ready",
            Duration::from_secs(1),
            futures::future::ready(Err::<(), _>(Error::TransportNotReady {
                state: TransportConnectionState::Disconnected,
                channel_open: true,
            })),
        )
        .await
        .is_none());
    assert!(stabilizer
        .run_step(
            "transient_miss_did",
            Duration::from_secs(1),
            futures::future::ready(Err::<(), _>(Error::SwarmMissDidInTable(peer))),
        )
        .await
        .is_none());
    assert!(stabilizer
        .run_step(
            "non_transient_lock_poisoned",
            Duration::from_secs(1),
            futures::future::ready(Err::<(), _>(Error::LockPoisoned)),
        )
        .await
        .is_none());

    logs_assert(|lines: &[&str]| {
        let step_failures = lines
            .iter()
            .filter(|line| line.contains("STABILIZATION step failed"))
            .copied()
            .collect::<Vec<_>>();
        let [transient_not_ready, transient_miss_did, non_transient] = step_failures.as_slice()
        else {
            return Err(format!(
                "expected 3 STABILIZATION step failed logs, found {}: {step_failures:?}",
                step_failures.len()
            ));
        };
        for line in [transient_not_ready, transient_miss_did] {
            if !line.contains("DEBUG") || line.contains("ERROR") {
                return Err(format!(
                    "expected DEBUG log for transient routing refusal: {line}"
                ));
            }
        }
        if !non_transient.contains("ERROR") {
            return Err(format!(
                "expected ERROR log for non-transient failure: {non_transient}"
            ));
        }
        Ok(())
    });
    Ok(())
}
