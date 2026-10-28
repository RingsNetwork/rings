//! The native daemon's deadline at its signing horizon (#1063).
//!
//! A node whose own delegation reaches its signing horizon `h = e − Δ` can send nothing, and
//! only a restart with a fresh delegation lets it send again (`processor::horizon`). Left
//! running, it would keep serving its local API while every peer dropped it, and a supervisor
//! would never notice. So the daemon ends at `h` with
//! [`Error::DelegationHorizonReached`], a non-zero exit that `Restart=always` or
//! `--restart unless-stopped` turns into a restart.
//!
//! ```text
//!   loop:  t ← now()
//!          Signing(r)       ─▶ sleep min(r − lead, P)
//!          Expiring(r)      ─▶ warn once;  sleep min(r, P)
//!          Mute             ─▶ return DelegationHorizonReached
//! ```
//!
//! The horizon is an instant of the epoch clock, while a sleep runs on the monotonic clock, which
//! stops while the machine is suspended. So no sleep is longer than the poll period
//! [`HORIZON_POLL`] (`P`), and every wake re-reads the epoch clock. A host suspended across the
//! horizon, or an epoch clock that stepped forward, ends the wait within `P` of resuming; one that
//! stepped back only sleeps again.

use std::time::Duration;

use crate::error::Error;
use crate::processor::DelegationHorizon;
use crate::processor::SigningState;
use crate::processor::SIGNING_HORIZON_LEAD_MS;

/// `P`: the longest single sleep of [`signing_horizon`], so a daemon whose host was suspended
/// across its horizon ends within one period of resuming (module documentation).
pub const HORIZON_POLL: Duration = Duration::from_secs(60);

/// Wait until `horizon` on the epoch clock `now_ms` (module diagram), warning once inside the
/// lead, and return the error that ends the daemon there. It never returns before the horizon.
pub async fn signing_horizon(horizon: DelegationHorizon, now_ms: impl Fn() -> u128) -> Error {
    let mut warned = false;
    loop {
        let now = now_ms();
        let wait_ms = match horizon.state_at(now) {
            SigningState::Mute => {
                tracing::error!(
                    expires_at_ms = %horizon.expires_at_ms(),
                    horizon_ms = %horizon.horizon_ms(),
                    "the node's delegation reached its signing horizon: no frame can leave, so \
                     the node exits for its supervisor to restart it with a fresh delegatee key"
                );
                return horizon.reached();
            }
            SigningState::Expiring { remaining_ms } => {
                if !warned {
                    warned = true;
                    tracing::warn!(
                        remaining_ms = %remaining_ms,
                        horizon_ms = %horizon.horizon_ms(),
                        "the node's delegation nears its signing horizon: the node exits then; \
                         rotate its delegatee key and restart it before"
                    );
                }
                remaining_ms
            }
            SigningState::Signing { remaining_ms } => remaining_ms - SIGNING_HORIZON_LEAD_MS,
        };
        let wait = Duration::from_millis(u64::try_from(wait_ms).unwrap_or(u64::MAX));
        tokio::time::sleep(wait.min(HORIZON_POLL)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::Instant;

    use super::signing_horizon;
    use super::HORIZON_POLL;
    use crate::error::Error;
    use crate::processor::DelegationHorizon;
    use crate::processor::SIGNING_HORIZON_LEAD_MS;

    /// The epoch reading at the paused runtime's start.
    const EPOCH_MS: u128 = 1_800_000_000_000;

    /// An epoch clock that reads the paused runtime's time from `EPOCH_MS`, plus `jump_ms` once
    /// `after` of it has passed: a host suspended at `after` for `jump_ms`, whose monotonic
    /// clock (the paused runtime's) did not advance meanwhile.
    fn injected_clock(after: Duration, jump_ms: u128) -> impl Fn() -> u128 {
        let start = Instant::now();
        move || {
            let elapsed = start.elapsed();
            let jumped = if elapsed >= after { jump_ms } else { 0 };
            EPOCH_MS + elapsed.as_millis() + jumped
        }
    }

    /// `signing → mute` (#1063): a delegation two leads past the start is waited out on the
    /// injected clock, in sleeps of at most `P`, and the daemon's error names the horizon at
    /// exactly that instant. The health predicate (`overlay_ready` reads `signs_at`) holds one
    /// millisecond before and fails at the end.
    #[tokio::test(start_paused = true)]
    async fn test_the_daemon_ends_at_its_horizon() {
        let now_ms = injected_clock(Duration::MAX, 0);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
        let h = horizon.horizon_ms();

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(
            error,
            Error::DelegationHorizonReached { horizon_ms, .. } if horizon_ms == h
        ));
        assert_eq!(now_ms(), h);
        assert!(horizon.signs_at(h - 1) && !horizon.signs_at(h));
    }

    /// A host suspended across the horizon (#1138 review): ten seconds in, the epoch clock jumps
    /// three leads, past the horizon, while the monotonic clock does not move. The daemon ends
    /// at its next wake, within one poll period of the start, not after the day-long sleep the
    /// monotonic clock alone would have waited.
    #[tokio::test(start_paused = true)]
    async fn test_a_daemon_suspended_across_its_horizon_ends_within_one_poll() {
        let suspended_at = Duration::from_secs(10);
        let now_ms = injected_clock(suspended_at, 3 * SIGNING_HORIZON_LEAD_MS);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS + 2 * SIGNING_HORIZON_LEAD_MS);
        let start = Instant::now();

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(error, Error::DelegationHorizonReached { .. }));
        assert!(start.elapsed() > suspended_at && start.elapsed() <= HORIZON_POLL);
    }

    /// A daemon already past its horizon ends at once, without waiting.
    #[tokio::test(start_paused = true)]
    async fn test_a_daemon_past_its_horizon_ends_at_once() {
        let now_ms = injected_clock(Duration::MAX, 0);
        let horizon = DelegationHorizon::expiring_at(EPOCH_MS);

        let error = signing_horizon(horizon, &now_ms).await;

        assert!(matches!(error, Error::DelegationHorizonReached { .. }));
        assert_eq!(now_ms(), EPOCH_MS);
    }
}
