//! Range-aware convergence state for the sparse Chord finger table.
//!
//! State relation:
//! - `verified[i]` means slot `i` was proved after its last hint change.
//! - `slot_epoch[i]` records that last change and prevents a range result from
//!   overwriting a slot changed after the lookup was issued.
//! - `in_flight` contains at most one request; only its exact token may apply.
//! - `last_issued_at_ms` enforces a hard per-node emission interval on the
//!   process-monotonic clock supplied by the effect boundary.
//! - `failure_streak` and `retry_not_before_ms` make loss, invalid reports,
//!   timeouts, and send cancellation progress-sensitive. Only a proved range
//!   resets the streak; the runtime scheduler adds boot-randomized full-window
//!   jitter and never performs catch-up bursts.

use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::topology::dist;
use crate::dht::Did;

/// Minimum process-monotonic separation between automatic finger lookup emissions.
pub(crate) const FINGER_LOOKUP_MIN_INTERVAL_MS: u64 = 1_000;

/// Maximum base delay between retries after repeated failures.
pub(crate) const FINGER_LOOKUP_MAX_BACKOFF_MS: u64 = 60_000;

/// Time after which an unanswered finger lookup no longer blocks convergence.
const FINGER_LOOKUP_TIMEOUT_MS: u64 = 10_000;

const FINGER_LOOKUP_MAX_BACKOFF_EXPONENT: u8 = 6;

/// Exponential retry floor for one node after `failure_streak` failures.
pub(crate) fn finger_lookup_backoff_ms(failure_streak: u8) -> u64 {
    let exponent = u32::from(failure_streak.min(FINGER_LOOKUP_MAX_BACKOFF_EXPONENT));
    FINGER_LOOKUP_MIN_INTERVAL_MS
        .checked_shl(exponent)
        .unwrap_or(FINGER_LOOKUP_MAX_BACKOFF_MS)
        .min(FINGER_LOOKUP_MAX_BACKOFF_MS)
}

/// Correlation token for one range-aware finger lookup.
///
/// The token is echoed in the lookup report. A report may mutate local state
/// only while this exact request remains in flight, so a topology change or a
/// retry cannot be overwritten by an older result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct FingerFixRequest {
    pub(crate) slot: u16,
    pub(crate) request_id: uuid::Uuid,
}

impl FingerFixRequest {
    pub(crate) fn new(slot: usize, request_id: uuid::Uuid) -> Option<Self> {
        Some(Self {
            slot: u16::try_from(slot).ok()?,
            request_id,
        })
    }

    /// Lowest finger slot whose successor this request proves.
    pub const fn slot(self) -> u16 {
        self.slot
    }

    /// Fresh UUID request identifier supplied by the effect boundary.
    pub const fn request_id(self) -> uuid::Uuid {
        self.request_id
    }

    pub(crate) fn slot_index(self) -> usize {
        usize::from(self.slot)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct PendingFingerLookup {
    request: FingerFixRequest,
    issued_epoch: u64,
    expires_at_ms: u64,
}

/// Scheduler-visible projection of the convergence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceStatus {
    pending: bool,
    failure_streak: u8,
}

impl FingerConvergenceStatus {
    pub(crate) const fn new(pending: bool, failure_streak: u8) -> Self {
        Self {
            pending,
            failure_streak,
        }
    }

    pub(crate) const fn inactive() -> Self {
        Self::new(false, 0)
    }

    pub(crate) const fn pending(self) -> bool {
        self.pending
    }

    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FingerConvergenceState {
    epoch: u64,
    slot_epoch: Vec<u64>,
    pub(crate) verified: Vec<bool>,
    in_flight: Option<PendingFingerLookup>,
    last_issued_at_ms: Option<u64>,
    failure_streak: u8,
    retry_not_before_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerResultDisposition {
    Applied { end: usize },
    Invalid,
    Stale,
}

/// Test-only projection used to model-check the production retry transition.
#[cfg(all(test, not(target_family = "wasm")))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceProjection {
    pub(crate) in_flight: Option<FingerFixRequest>,
    pub(crate) expires_at_ms: Option<u64>,
    pub(crate) last_issued_at_ms: Option<u64>,
    pub(crate) failure_streak: u8,
    pub(crate) retry_not_before_ms: Option<u64>,
}

impl FingerConvergenceState {
    pub(crate) fn new(slot_count: usize) -> Self {
        Self {
            epoch: 0,
            slot_epoch: vec![0; slot_count],
            verified: vec![false; slot_count],
            in_flight: None,
            last_issued_at_ms: None,
            failure_streak: 0,
            retry_not_before_ms: None,
        }
    }

    pub(crate) fn normalized(mut self, slot_count: usize) -> Self {
        self.slot_epoch.resize(slot_count, self.epoch);
        self.verified.resize(slot_count, false);
        if self
            .in_flight
            .is_some_and(|pending| pending.request.slot_index() >= slot_count)
        {
            self.in_flight = None;
        }
        self
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.in_flight.is_some() || self.verified.iter().any(|verified| !verified)
    }

    pub(crate) fn status(&self) -> FingerConvergenceStatus {
        FingerConvergenceStatus::new(self.is_pending(), self.failure_streak)
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn projection(&self) -> FingerConvergenceProjection {
        FingerConvergenceProjection {
            in_flight: self.in_flight.map(|pending| pending.request),
            expires_at_ms: self.in_flight.map(|pending| pending.expires_at_ms),
            last_issued_at_ms: self.last_issued_at_ms,
            failure_streak: self.failure_streak,
            retry_not_before_ms: self.retry_not_before_ms,
        }
    }

    fn record_failure(&mut self, now_ms: u64) {
        self.failure_streak = self.failure_streak.saturating_add(1);
        self.retry_not_before_ms =
            Some(now_ms.saturating_add(finger_lookup_backoff_ms(self.failure_streak)));
    }

    fn record_progress(&mut self) {
        self.failure_streak = 0;
        self.retry_not_before_ms = None;
    }

    fn request_is_current(&self, request: FingerFixRequest) -> bool {
        self.in_flight
            .is_some_and(|pending| pending.request == request)
    }

    pub(crate) fn invalidate_hint_changes(
        &mut self,
        before: &[Option<Did>],
        after: &[Option<Did>],
    ) {
        let changed = before
            .iter()
            .zip(after)
            .map(|(before, after)| before != after)
            .collect::<Vec<_>>();
        if !changed.iter().any(|changed| *changed) {
            return;
        }

        let Some(next_epoch) = self.epoch.checked_add(1) else {
            self.in_flight = None;
            self.verified.fill(false);
            return;
        };
        self.epoch = next_epoch;
        for ((slot_epoch, verified), changed) in self
            .slot_epoch
            .iter_mut()
            .zip(&mut self.verified)
            .zip(&changed)
        {
            if *changed {
                *slot_epoch = next_epoch;
                *verified = false;
            }
        }

        let invalidated_in_flight = self.in_flight.is_some_and(|pending| {
            changed
                .get(pending.request.slot_index())
                .copied()
                .unwrap_or(true)
        });
        if invalidated_in_flight {
            self.in_flight = None;
            // This transition has no clock input. Anchor the pure retry floor
            // at the last emission; the runtime scheduler observes the higher
            // failure level and adds a fresh full-jitter delay from its current
            // monotonic time.
            self.record_failure(self.last_issued_at_ms.unwrap_or(0));
        }
    }

    pub(crate) fn invalidate_all_evidence(&mut self) {
        if self.in_flight.is_none() && self.verified.iter().all(|verified| !verified) {
            return;
        }

        if let Some(next_epoch) = self.epoch.checked_add(1) {
            self.epoch = next_epoch;
            self.slot_epoch.fill(next_epoch);
        }
        let invalidated_in_flight = self.in_flight.take().is_some();
        self.verified.fill(false);
        if invalidated_in_flight {
            self.record_failure(self.last_issued_at_ms.unwrap_or(0));
        }
    }

    fn refresh_next_range(&mut self, fingers: &[Option<Did>], cursor: usize) {
        let slot_count = fingers.len();
        if slot_count == 0 {
            return;
        }
        let start = cursor.saturating_add(1) % slot_count;
        let Some(value) = fingers.get(start).copied() else {
            return;
        };
        for (finger, verified) in fingers
            .iter()
            .skip(start)
            .zip(self.verified.iter_mut().skip(start))
        {
            if *finger != value {
                break;
            }
            *verified = false;
        }
    }

    pub(crate) fn begin_revalidation(&mut self, fingers: &[Option<Did>], cursor: usize) {
        if self.in_flight.is_none() && self.verified.iter().all(|verified| *verified) {
            self.refresh_next_range(fingers, cursor);
        }
    }

    pub(crate) fn prepare_lookup(
        &mut self,
        fingers: &[Option<Did>],
        now_ms: u64,
        request_id: uuid::Uuid,
    ) -> Option<FingerFixRequest> {
        if fingers.is_empty() {
            return None;
        }
        if self
            .in_flight
            .is_some_and(|pending| now_ms >= pending.expires_at_ms)
        {
            self.in_flight = None;
            self.record_failure(now_ms);
            return None;
        }
        if self.in_flight.is_some() {
            return None;
        }
        if self.verified.iter().all(|verified| *verified) {
            return None;
        }
        if self
            .last_issued_at_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < FINGER_LOOKUP_MIN_INTERVAL_MS)
        {
            return None;
        }
        if self
            .retry_not_before_ms
            .is_some_and(|deadline| now_ms < deadline)
        {
            return None;
        }

        let slot = self.verified.iter().position(|verified| !verified)?;
        let request = FingerFixRequest::new(slot, request_id)?;
        self.in_flight = Some(PendingFingerLookup {
            request,
            issued_epoch: self.epoch,
            expires_at_ms: now_ms.saturating_add(FINGER_LOOKUP_TIMEOUT_MS),
        });
        self.last_issued_at_ms = Some(now_ms);
        Some(request)
    }

    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn prepare_slot_for_test(
        &mut self,
        fingers: &[Option<Did>],
        slot: usize,
        now_ms: u64,
        request_id: uuid::Uuid,
    ) -> Option<FingerFixRequest> {
        self.verified.fill(true);
        *self.verified.get_mut(slot)? = false;
        self.in_flight = None;
        self.last_issued_at_ms = None;
        self.prepare_lookup(fingers, now_ms, request_id)
    }

    pub(crate) fn result_disposition(
        &self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
    ) -> FingerResultDisposition {
        let Some(pending) = self.in_flight.filter(|pending| pending.request == request) else {
            return FingerResultDisposition::Stale;
        };
        let Some(end) = finger_proof_end(local, successor, request.slot_index(), slot_count) else {
            return FingerResultDisposition::Invalid;
        };
        let applicable = self
            .slot_epoch
            .iter()
            .skip(request.slot_index())
            .take(end.saturating_sub(request.slot_index()).saturating_add(1))
            .any(|slot_epoch| *slot_epoch <= pending.issued_epoch);
        if applicable {
            FingerResultDisposition::Applied { end }
        } else {
            FingerResultDisposition::Stale
        }
    }

    pub(crate) fn apply_result(
        &mut self,
        local: Did,
        fingers: &mut [Option<Did>],
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerResultDisposition {
        let Some(pending) = self.in_flight.filter(|pending| pending.request == request) else {
            return FingerResultDisposition::Stale;
        };
        self.in_flight = None;
        let Some(end) = finger_proof_end(local, successor, request.slot_index(), fingers.len())
        else {
            self.record_failure(now_ms);
            return FingerResultDisposition::Invalid;
        };
        let replacement = (successor != local).then_some(successor);
        let count = end.saturating_sub(request.slot_index()).saturating_add(1);
        let mut applied = false;
        for ((finger, slot_epoch), verified) in fingers
            .iter_mut()
            .zip(&self.slot_epoch)
            .zip(&mut self.verified)
            .skip(request.slot_index())
            .take(count)
        {
            if *slot_epoch <= pending.issued_epoch {
                *finger = replacement;
                *verified = true;
                applied = true;
            }
        }
        if applied {
            self.record_progress();
            FingerResultDisposition::Applied { end }
        } else {
            self.record_failure(now_ms);
            FingerResultDisposition::Stale
        }
    }

    pub(crate) fn cancel(&mut self, request: FingerFixRequest, now_ms: u64) {
        if self.request_is_current(request) {
            self.in_flight = None;
            self.record_failure(now_ms);
        }
    }
}

fn finger_proof_end(local: Did, successor: Did, start: usize, slot_count: usize) -> Option<usize> {
    let last = slot_count.checked_sub(1)?;
    if start > last {
        return None;
    }
    if successor == local {
        return Some(last);
    }
    let distance = dist(local, successor);
    if distance < (BigUint::from(1u8) << start) {
        return None;
    }
    let highest = usize::try_from(distance.bits().saturating_sub(1)).ok()?;
    Some(highest.min(last))
}
