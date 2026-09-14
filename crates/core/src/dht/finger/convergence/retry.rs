//! Per-node pacing and retry state for background finger maintenance.
//!
//! This layer does not schedule timers or generate randomness. It records the
//! deterministic lower bound that the outer maintenance scheduler must honor.
//! The scheduler adds lifecycle-derived full-window jitter and browser-resume
//! rephasing without changing this state relation.
//!
//! These concrete delays are operational policy, not a Chord theorem. They
//! bound repeated failure traffic while allowing eventual retries; changing
//! them requires re-running the network-effect and browser lifecycle tests.

use serde::Deserialize;
use serde::Serialize;

/// Minimum process-monotonic separation between automatic finger emissions.
pub(crate) const FINGER_LOOKUP_MIN_INTERVAL_MS: u64 = 1_000;

/// Maximum deterministic retry floor after repeated failures.
pub(crate) const FINGER_LOOKUP_MAX_BACKOFF_MS: u64 = 60_000;

/// Highest shift applied before the retry floor is capped.
const FINGER_LOOKUP_MAX_BACKOFF_EXPONENT: u8 = 6;

/// Exponential retry floor after `failure_streak` consecutive failures.
///
/// The first failure waits two seconds. Later failures wait 4, 8, 16, 32,
/// then at most 60 seconds. The outer scheduler adds a jitter window of the
/// same size, so this value is a floor rather than an exact retry timestamp.
pub(crate) fn finger_lookup_backoff_ms(failure_streak: u8) -> u64 {
    let exponent = u32::from(failure_streak.min(FINGER_LOOKUP_MAX_BACKOFF_EXPONENT));
    FINGER_LOOKUP_MIN_INTERVAL_MS
        .checked_shl(exponent)
        .unwrap_or(FINGER_LOOKUP_MAX_BACKOFF_MS)
        .min(FINGER_LOOKUP_MAX_BACKOFF_MS)
}

/// Deterministic retry state for automatic finger lookups.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct FingerRetryState {
    /// Last automatic lookup emission, used for the hard minimum interval.
    last_issued_at_ms: Option<u64>,
    /// Consecutive current-attempt failures since the last committed progress.
    failure_streak: u8,
    /// Earliest monotonic timestamp at which failure retry may be attempted.
    retry_not_before_ms: Option<u64>,
}

impl FingerRetryState {
    /// Return whether both the hard emission interval and failure floor allow
    /// a new lookup at `now_ms`.
    pub(super) fn permits_issue(self, now_ms: u64) -> bool {
        let interval_elapsed = self
            .last_issued_at_ms
            .is_none_or(|last| now_ms.saturating_sub(last) >= FINGER_LOOKUP_MIN_INTERVAL_MS);
        let retry_elapsed = self
            .retry_not_before_ms
            .is_none_or(|deadline| now_ms >= deadline);
        interval_elapsed && retry_elapsed
    }

    /// Remember that a lookup was emitted at `now_ms`.
    pub(super) fn record_issue(&mut self, now_ms: u64) {
        self.last_issued_at_ms = Some(now_ms);
    }

    /// Increase retry pressure and set the next deterministic retry floor.
    pub(super) fn record_failure(&mut self, now_ms: u64) {
        self.failure_streak = self.failure_streak.saturating_add(1);
        self.retry_not_before_ms =
            Some(now_ms.saturating_add(finger_lookup_backoff_ms(self.failure_streak)));
    }

    /// Clear failure backoff after a proof or equivalent local evidence lands.
    pub(super) fn record_progress(&mut self) {
        self.failure_streak = 0;
        self.retry_not_before_ms = None;
    }

    /// Number of consecutive current-attempt failures.
    pub(super) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }

    /// Project private retry fields for model assertions.
    #[cfg(test)]
    pub(super) const fn projection(self) -> (Option<u64>, u8, Option<u64>) {
        (
            self.last_issued_at_ms,
            self.failure_streak,
            self.retry_not_before_ms,
        )
    }

    /// Clear the hard emission interval in tests that manually prepare slots.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(super) fn clear_last_issue(&mut self) {
        self.last_issued_at_ms = None;
    }
}
