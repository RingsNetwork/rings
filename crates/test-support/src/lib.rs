//! Runtime-neutral test support shared by the Rings crates' test suites.
//!
//! It is a dev-dependency only (`publish = false`). It holds the parts of the test harnesses
//! that do not depend on any Rings type, so that core, node and the browser tests share one
//! definition:
//!
//! - [`activity`]: the process-wide activity generation that makes every test wait
//!   *state-driven*. A wait probes the state it needs and is woken by the next state change,
//!   never by a timer.
//! - [`with_hang_guard`]: the per-test failure bound that names a hung test instead of letting
//!   it consume a shared runner budget.
//!
//! Neither needs a particular runtime: timers come from [`rings_runtime::sleep`], so the same
//! code runs on tokio and on the browser's event loop.

pub mod activity;

use std::future::Future;
use std::time::Duration;

use futures::FutureExt;

/// Run `test` under a hang guard of `budget`, failing with `name` once it has elapsed.
///
/// The guard is a failure bound only: a passing run proceeds on `test` alone, since every wait
/// inside a test is on an observed state. A caller whose test still paces itself on durations
/// passes a *scenario budget* instead and must name it as such.
pub async fn with_hang_guard<T>(name: &str, budget: Duration, test: impl Future<Output = T>) -> T {
    let test = test.fuse();
    let deadline = rings_runtime::sleep(budget).fuse();
    futures::pin_mut!(test, deadline);
    futures::select! {
        value = test => value,
        _ = deadline => panic!("{name} exceeded its {budget:?} hang guard"),
    }
}
