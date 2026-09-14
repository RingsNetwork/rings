//! Range-aware convergence state for the sparse Chord finger table.
//!
//! State relation:
//! - `verified[i]` means slot `i` was proved after its last hint change.
//! - `slot_epoch[i]` records that last change and prevents a range result from
//!   overwriting a slot changed after the lookup was issued.
//! - Exactly one lookup may be `in_flight` or retained as a `deferred` proof;
//!   only its exact token may apply.
//! - `last_issued_at_ms` enforces a hard per-node emission interval on the
//!   process-monotonic clock supplied by the effect boundary.
//! - `failure_streak` and `retry_not_before_ms` make loss, invalid reports,
//!   timeouts, and send cancellation progress-sensitive. Only a proved range
//!   resets the streak; the runtime scheduler adds node-lifecycle-randomized
//!   full-window jitter, rephases stale browser-resume deadlines, and never
//!   performs catch-up bursts.

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

/// Maximum time a timely proof may wait for transport admission. This matches
/// the pending WebRTC generation lease while also covering the interval before
/// a generation has been allocated.
pub(crate) const FINGER_ADMISSION_TIMEOUT_MS: u64 = 180_000;

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
/// only while this exact request remains in flight or is retained for its
/// candidate's admission, so a topology change or retry cannot be overwritten
/// by an older result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct PendingFingerLookup {
    request: FingerFixRequest,
    issued_epoch: u64,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct DeferredFingerProof {
    request: FingerFixRequest,
    issued_epoch: u64,
    successor: Did,
    end: usize,
    expires_at_ms: u64,
}

/// Scheduler-visible phase of one node's finger convergence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerConvergencePhase {
    /// No automatic work is currently required.
    Inactive,
    /// An unverified range can issue a lookup when its paced deadline arrives.
    Runnable,
    /// One emitted lookup is waiting for its report for at most this much longer.
    ///
    /// This is a remaining duration, rather than the lookup clock's absolute
    /// timestamp, so a restarted maintenance listener cannot interpret it in
    /// a different monotonic-clock domain.
    AwaitingReport { remaining_ms: u64 },
    /// A timely report is retained until admission or its bounded lease expires.
    AwaitingAdmission { remaining_ms: u64 },
}

/// Scheduler-visible projection of the convergence state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceStatus {
    phase: FingerConvergencePhase,
    failure_streak: u8,
}

impl FingerConvergenceStatus {
    pub(crate) const fn new(pending: bool, failure_streak: u8) -> Self {
        Self {
            phase: if pending {
                FingerConvergencePhase::Runnable
            } else {
                FingerConvergencePhase::Inactive
            },
            failure_streak,
        }
    }

    pub(crate) const fn awaiting_report(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingReport { remaining_ms },
            failure_streak,
        }
    }

    pub(crate) const fn awaiting_admission(remaining_ms: u64, failure_streak: u8) -> Self {
        Self {
            phase: FingerConvergencePhase::AwaitingAdmission { remaining_ms },
            failure_streak,
        }
    }

    pub(crate) const fn inactive() -> Self {
        Self::new(false, 0)
    }

    #[cfg(test)]
    pub(crate) const fn pending(self) -> bool {
        !matches!(self.phase, FingerConvergencePhase::Inactive)
    }

    pub(crate) const fn phase(self) -> FingerConvergencePhase {
        self.phase
    }

    pub(crate) const fn may_advance(self) -> bool {
        matches!(
            self.phase,
            FingerConvergencePhase::Runnable
                | FingerConvergencePhase::AwaitingReport { .. }
                | FingerConvergencePhase::AwaitingAdmission { .. }
        )
    }

    pub(crate) const fn failure_streak(self) -> u8 {
        self.failure_streak
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct FingerConvergenceState {
    epoch: u64,
    slot_epoch: Vec<u64>,
    pub(crate) verified: Vec<bool>,
    in_flight: Option<PendingFingerLookup>,
    deferred: Option<DeferredFingerProof>,
    last_issued_at_ms: Option<u64>,
    failure_streak: u8,
    retry_not_before_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerResultDisposition {
    Applied { end: usize },
    Invalid,
    Expired,
    Stale,
}

/// Test-only projection used to model-check the production retry transition.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FingerConvergenceProjection {
    pub(crate) verified: Vec<bool>,
    pub(crate) in_flight: Option<FingerFixRequest>,
    pub(crate) deferred: Option<FingerFixRequest>,
    pub(crate) deferred_expires_at_ms: Option<u64>,
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
            deferred: None,
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
        if self
            .deferred
            .is_some_and(|proof| proof.request.slot_index() >= slot_count)
        {
            self.deferred = None;
        } else if let Some(proof) = &mut self.deferred {
            proof.end = proof.end.min(slot_count.saturating_sub(1));
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn is_pending(&self) -> bool {
        self.in_flight.is_some()
            || self.deferred.is_some()
            || self.verified.iter().any(|verified| !verified)
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> FingerConvergenceStatus {
        self.status_after(0, 0)
    }

    pub(crate) fn status_after(
        &self,
        first_routable_slot: usize,
        now_ms: u64,
    ) -> FingerConvergenceStatus {
        if let Some(pending) = self.in_flight {
            FingerConvergenceStatus::awaiting_report(
                pending.expires_at_ms.saturating_sub(now_ms),
                self.failure_streak,
            )
        } else if let Some(proof) = self.deferred {
            FingerConvergenceStatus::awaiting_admission(
                proof.expires_at_ms.saturating_sub(now_ms),
                self.failure_streak,
            )
        } else {
            FingerConvergenceStatus::new(
                self.verified
                    .iter()
                    .skip(first_routable_slot)
                    .any(|verified| !verified),
                self.failure_streak,
            )
        }
    }

    #[cfg(test)]
    pub(crate) fn projection(&self) -> FingerConvergenceProjection {
        FingerConvergenceProjection {
            verified: self.verified.clone(),
            in_flight: self.in_flight.map(|pending| pending.request),
            deferred: self.deferred.map(|proof| proof.request),
            deferred_expires_at_ms: self.deferred.map(|proof| proof.expires_at_ms),
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
            || self.deferred.is_some_and(|proof| proof.request == request)
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
            self.deferred = None;
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
        }
        let invalidated_deferred = self.deferred.is_some_and(|proof| {
            changed
                .get(proof.request.slot_index())
                .copied()
                .unwrap_or(true)
        });
        if invalidated_deferred {
            self.deferred = None;
        }
    }

    pub(crate) fn invalidate_all_evidence(&mut self) {
        if self.in_flight.is_none()
            && self.deferred.is_none()
            && self.verified.iter().all(|verified| !verified)
        {
            return;
        }

        if let Some(next_epoch) = self.epoch.checked_add(1) {
            self.epoch = next_epoch;
            self.slot_epoch.fill(next_epoch);
        }
        self.in_flight = None;
        self.deferred = None;
        self.verified.fill(false);
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
        if self.in_flight.is_none()
            && self.deferred.is_none()
            && self.verified.iter().all(|verified| *verified)
        {
            self.refresh_next_range(fingers, cursor);
        }
    }

    pub(crate) fn prepare_lookup(
        &mut self,
        fingers: &[Option<Did>],
        first_slot: usize,
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
        if self
            .deferred
            .is_some_and(|proof| now_ms >= proof.expires_at_ms)
        {
            self.deferred = None;
            self.record_failure(now_ms);
            return None;
        }
        if self.in_flight.is_some() || self.deferred.is_some() {
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

        let slot = self
            .verified
            .iter()
            .enumerate()
            .skip(first_slot)
            .find_map(|(slot, verified)| (!verified).then_some(slot))?;
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
        self.deferred = None;
        self.prepare_lookup(fingers, 0, now_ms, request_id)
    }

    fn checked_report(
        &self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> Result<(PendingFingerLookup, usize), FingerResultDisposition> {
        let pending = self
            .in_flight
            .filter(|pending| pending.request == request)
            .ok_or(FingerResultDisposition::Stale)?;
        if now_ms >= pending.expires_at_ms {
            return Err(FingerResultDisposition::Expired);
        }
        let end = finger_proof_end(local, successor, request.slot_index(), slot_count)
            .ok_or(FingerResultDisposition::Invalid)?;
        let applicable = self
            .slot_epoch
            .iter()
            .skip(request.slot_index())
            .take(end.saturating_sub(request.slot_index()).saturating_add(1))
            .any(|slot_epoch| *slot_epoch <= pending.issued_epoch);
        if applicable {
            Ok((pending, end))
        } else {
            Err(FingerResultDisposition::Stale)
        }
    }

    pub(crate) fn result_disposition(
        &self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerResultDisposition {
        if let Some(proof) = self
            .deferred
            .filter(|proof| proof.request == request && proof.successor == successor)
        {
            return if now_ms >= proof.expires_at_ms {
                FingerResultDisposition::Expired
            } else {
                FingerResultDisposition::Applied { end: proof.end }
            };
        }
        self.checked_report(local, slot_count, request, successor, now_ms)
            .map_or_else(
                |disposition| disposition,
                |(_, end)| FingerResultDisposition::Applied { end },
            )
    }

    pub(crate) fn defer_result(
        &mut self,
        local: Did,
        slot_count: usize,
        request: FingerFixRequest,
        successor: Did,
        now_ms: u64,
    ) -> FingerResultDisposition {
        if let Some(proof) = self
            .deferred
            .filter(|proof| proof.request == request && proof.successor == successor)
        {
            if now_ms >= proof.expires_at_ms {
                self.deferred = None;
                self.record_failure(now_ms);
                return FingerResultDisposition::Expired;
            }
            return FingerResultDisposition::Applied { end: proof.end };
        }
        match self.checked_report(local, slot_count, request, successor, now_ms) {
            Ok((pending, end)) => {
                self.in_flight = None;
                self.deferred = Some(DeferredFingerProof {
                    request,
                    issued_epoch: pending.issued_epoch,
                    successor,
                    end,
                    expires_at_ms: now_ms.saturating_add(FINGER_ADMISSION_TIMEOUT_MS),
                });
                FingerResultDisposition::Applied { end }
            }
            Err(disposition) => {
                if self
                    .in_flight
                    .is_some_and(|pending| pending.request == request)
                {
                    self.in_flight = None;
                    if matches!(
                        disposition,
                        FingerResultDisposition::Expired | FingerResultDisposition::Invalid
                    ) {
                        self.record_failure(now_ms);
                    }
                }
                disposition
            }
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
        let validated = if let Some(proof) = self
            .deferred
            .filter(|proof| proof.request == request && proof.successor == successor)
        {
            self.deferred = None;
            if now_ms >= proof.expires_at_ms {
                self.record_failure(now_ms);
                Err(FingerResultDisposition::Expired)
            } else {
                Ok((proof.issued_epoch, proof.end))
            }
        } else {
            match self.checked_report(local, fingers.len(), request, successor, now_ms) {
                Ok((pending, end)) => {
                    self.in_flight = None;
                    Ok((pending.issued_epoch, end))
                }
                Err(disposition) => {
                    if self
                        .in_flight
                        .is_some_and(|pending| pending.request == request)
                    {
                        self.in_flight = None;
                        if matches!(
                            disposition,
                            FingerResultDisposition::Expired | FingerResultDisposition::Invalid
                        ) {
                            self.record_failure(now_ms);
                        }
                    }
                    Err(disposition)
                }
            }
        };
        let (issued_epoch, end) = match validated {
            Ok(validated) => validated,
            Err(disposition) => return disposition,
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
            if *slot_epoch <= issued_epoch {
                *finger = replacement;
                *verified = true;
                applied = true;
            }
        }
        if applied {
            self.record_progress();
            FingerResultDisposition::Applied { end }
        } else {
            FingerResultDisposition::Stale
        }
    }

    pub(crate) fn confirm_range(&mut self, start: usize, end: usize) -> bool {
        if start > end || start >= self.verified.len() {
            return false;
        }
        let end = end.min(self.verified.len().saturating_sub(1));
        let newly_verified = self
            .verified
            .iter()
            .skip(start)
            .take(end.saturating_sub(start).saturating_add(1))
            .any(|verified| !verified);
        self.verified
            .iter_mut()
            .skip(start)
            .take(end.saturating_sub(start).saturating_add(1))
            .for_each(|verified| *verified = true);
        let superseded_in_flight = self
            .in_flight
            .is_some_and(|pending| (start..=end).contains(&pending.request.slot_index()));
        if superseded_in_flight {
            self.in_flight = None;
        }
        let superseded_deferred = self
            .deferred
            .is_some_and(|proof| (start..=end).contains(&proof.request.slot_index()));
        if superseded_deferred {
            self.deferred = None;
        }
        let progressed = newly_verified || superseded_in_flight || superseded_deferred;
        if progressed {
            self.record_progress();
        }
        progressed
    }

    pub(crate) fn cancel(&mut self, request: FingerFixRequest, now_ms: u64) {
        if self.request_is_current(request) {
            self.in_flight = None;
            self.deferred = None;
            self.record_failure(now_ms);
        }
    }
}

pub(crate) fn finger_proof_end(
    local: Did,
    successor: Did,
    start: usize,
    slot_count: usize,
) -> Option<usize> {
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
