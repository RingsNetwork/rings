//! Chord finger-range proofs and their externally visible outcomes.
//!
//! Chord numbers finger slots from powers of two. Rings uses zero-based slots,
//! so slot `i` asks for `succ(local + 2^i)`. Suppose that lookup returns `s`
//! and the clockwise distance from `local` to `s` is `d`. By the definition of
//! `succ`, the interval `[local + 2^i, s)` contains no admitted member. Hence
//! every later target `local + 2^j` with `2^j <= d` has the same successor.
//! One lookup therefore proves the consecutive range
//!
//! `i ..= floor(log2(d))`.
//!
//! This is a lemma derived from the Chord finger definition, not a new routing
//! assumption. This module computes only the geometric bound. Correlation,
//! topology epochs, expiry, and transport admission are separate layers.
//!
//! Algorithmic source: Stoica et al., *Chord: A Scalable Peer-to-peer Lookup
//! Service for Internet Applications*, section 4 defines
//! `finger[k] = successor(n + 2^(k-1))`:
//! <https://pdos.csail.mit.edu/papers/ton:chord/paper-ton.pdf>. The paper does
//! not specify this batched maintenance state machine; the consecutive-range
//! lemma above is a direct consequence of that definition. Zave's correctness
//! work explains why fingers remain an optimization rather than a ring-safety
//! premise: <https://arxiv.org/abs/1502.06461>.

use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::topology::dist;
use crate::dht::Did;

/// Correlation token for one range-aware finger lookup.
///
/// A fresh UUID is allocated at the effect boundary and echoed by the report.
/// The slot identifies the lower end of the range being proved. A report can
/// mutate state only while this exact pair remains owned by the local state
/// machine.
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

    /// Fresh UUID allocated for this lookup.
    pub const fn request_id(self) -> uuid::Uuid {
        self.request_id
    }

    pub(crate) fn slot_index(self) -> usize {
        usize::from(self.slot)
    }
}

/// Why a reported finger proof was not accepted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerReportRejection {
    /// The returned successor cannot own the requested target.
    Invalid,
    /// The report or retained admission proof exceeded its deadline.
    Expired,
    /// The token or topology epoch no longer belongs to the current state.
    Stale,
}

impl FingerReportRejection {
    /// Invalid and expired current attempts are network failures; stale
    /// reports are harmless reorderings and must not increase retry pressure.
    pub(super) const fn counts_as_failure(self) -> bool {
        matches!(self, Self::Invalid | Self::Expired)
    }
}

/// Outcome of committing a report to the finger table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerApplyOutcome {
    /// At least one still-current slot in this proved range was committed.
    Applied { end: usize },
    /// The report was rejected without changing a finger hint.
    Rejected(FingerReportRejection),
}

/// Outcome of retaining a report for transport admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerDeferOutcome {
    /// The timely proof is owned by the admission lease through this slot.
    Deferred { end: usize },
    /// The report was rejected and no admission work should start.
    Rejected(FingerReportRejection),
}

/// Outcome of retiring a reported candidate that transport cannot use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerRetireOutcome {
    /// The exact, still-current report was retired into retry backoff.
    Retired,
    /// Candidate validation failed. A stale conflicting report is left without
    /// effect; an invalid or expired current report retires into backoff.
    Rejected(FingerReportRejection),
}

/// Validated range carried between the attempt and evidence layers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct FingerRangeProof {
    pub(super) request: FingerFixRequest,
    pub(super) issued_epoch: u64,
    pub(super) successor: Did,
    pub(super) end: usize,
}

impl FingerRangeProof {
    /// Number of consecutive slots carried by this proof.
    ///
    /// Checked subtraction makes restored local state fail closed if its range
    /// is ever malformed; a valid proof always has `end >= request.slot`.
    pub(super) fn covered_slot_count(self) -> usize {
        self.end
            .checked_sub(self.request.slot_index())
            .map_or(0, |distance| distance.saturating_add(1))
    }
}

/// Return the inclusive upper slot proved by one successor lookup.
///
/// Preconditions:
/// - `start < slot_count`;
/// - `successor` is the authenticated answer for `local + 2^start`.
///
/// Postcondition: every slot `j` in `start..=end` has the same successor in
/// the membership view that answered the lookup. Epoch validation performed by
/// the evidence layer prevents this proof from overwriting a later hint.
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
        // Returning to `local` means the successor interval crossed the ring
        // origin without encountering another member. Every no-wrap target at
        // or above `start` is therefore owned by `local` in this membership
        // view.
        return Some(last);
    }

    let distance = dist(local, successor);
    let first_target_distance = BigUint::from(1u8) << start;
    if distance < first_target_distance {
        return None;
    }

    // `bits(d) - 1 = floor(log2(d))` for every positive integer distance.
    let highest = usize::try_from(distance.bits().saturating_sub(1)).ok()?;
    Some(highest.min(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_bound_follows_the_chord_power_of_two_definition() {
        let local = Did::from(0u32);

        // Distance 7 covers targets 1, 2, and 4, but not target 8.
        assert_eq!(finger_proof_end(local, Did::from(7u32), 0, 8), Some(2));
        // Starting at target 4, distance 8 covers slots 2 and 3.
        assert_eq!(finger_proof_end(local, Did::from(8u32), 2, 8), Some(3));
        // Returning to local crosses the ring origin and proves the tail.
        assert_eq!(finger_proof_end(local, local, 5, 8), Some(7));
        // A predecessor of the requested target is not a proof for that slot.
        assert_eq!(finger_proof_end(local, Did::from(3u32), 2, 8), None);
        // Slot indices outside the configured table are rejected.
        assert_eq!(finger_proof_end(local, Did::from(8u32), 8, 8), None);
    }
}
