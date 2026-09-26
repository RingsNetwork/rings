//! Activity-woken state probes.
//!
//! A test never waits for a duration. It probes the state it needs, and when the state does not
//! hold yet, it awaits the next *activity*: a process-wide generation that every test node
//! advances whenever its observable state may have changed (each crate's test harness wires
//! its observers and callbacks to [`record_activity`]).
//!
//! ```text
//! probe:  loop { m := mark() ; probe() = Some(v) ? return v : await generation ≠ m }
//! ```
//!
//! Law (no lost wake-up): the generation is marked before the probe, so a change that lands
//! after the probe advances the generation past the mark and wakes the next wait. Activity from
//! other tests in the same process only causes extra probes. The cell is a plain mutex and a
//! waker list, so it needs no runtime and works on the browser's single thread as well.

use std::future::poll_fn;
use std::future::Future;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::task::Poll;
use std::task::Waker;
use std::time::Duration;

use crate::with_hang_guard;

/// The activity generation and the tasks waiting for it to advance.
struct ActivityState {
    generation: u64,
    waiters: Vec<Waker>,
}

/// The process-wide activity cell.
static ACTIVITY: Mutex<ActivityState> = Mutex::new(ActivityState {
    generation: 0,
    waiters: Vec::new(),
});

/// Lock the activity cell; a panicked test cannot poison the others' waits.
fn activity() -> MutexGuard<'static, ActivityState> {
    ACTIVITY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Record that some test node's observable state may have changed, and wake every waiter.
pub fn record_activity() {
    let waiters = {
        let mut state = activity();
        state.generation = state.generation.wrapping_add(1);
        std::mem::take(&mut state.waiters)
    };
    waiters.into_iter().for_each(Waker::wake);
}

/// The current activity generation, to be marked before a probe.
pub fn activity_mark() -> u64 {
    activity().generation
}

/// Resolve once the activity generation differs from `mark`.
pub async fn activity_after(mark: u64) {
    poll_fn(|context| {
        let mut state = activity();
        if state.generation != mark {
            return Poll::Ready(());
        }
        if !state
            .waiters
            .iter()
            .any(|waiter| waiter.will_wake(context.waker()))
        {
            state.waiters.push(context.waker().clone());
        }
        Poll::Pending
    })
    .await
}

/// Probe `probe` on every activity until it yields `Some`, failing with `label` once
/// `hang_guard` has elapsed; an `Err` from the probe ends the wait with that error.
///
/// The guard is a failure bound only: a passing run proceeds on an observed state, never on
/// elapsed time.
pub async fn probe_on_activity<T, E, F>(
    label: &str,
    hang_guard: Duration,
    mut probe: impl FnMut() -> F,
) -> Result<T, E>
where
    F: Future<Output = Result<Option<T>, E>>,
{
    let probing = async {
        loop {
            let mark = activity_mark();
            if let Some(value) = probe().await? {
                return Ok(value);
            }
            activity_after(mark).await;
        }
    };
    with_hang_guard(label, hang_guard, probing).await
}
