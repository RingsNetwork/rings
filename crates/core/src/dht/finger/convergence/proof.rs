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
//! This module computes only that geometric bound. Correlation, topology
//! epochs, expiry, and transport admission are separate layers.
//!
//! Algorithmic source: the lemma is Chord's own. Stoica et al., *Chord: A
//! Scalable Peer-to-peer Lookup Service for Internet Applications*, SIGCOMM
//! 2001, section 4.4, Figure 6, `init_finger_table`: node `n` checks whether
//! `finger[i].node` is also the correct `finger[i+1]` entry, which holds
//! exactly when `finger[i].interval` contains no node, and so
//! `finger[i].node >= finger[i+1].start`; the paper derives the `O(log N)`
//! expected-lookup bound from it. The same section's advice to copy a
//! neighbour's finger table as initial hints is the source of the
//! inferred-hint model that the `evidence` module versions.
//! <https://pdos.csail.mit.edu/papers/chord:sigcomm01/chord_sigcomm.pdf>. The
//! IEEE/ACM ToN 2003 revision dropped both in favour of randomized
//! `fix_fingers`, which is why the batching is easy to mistake for a new
//! result. Rings-specific are only the evidence epochs, UUID correlation,
//! admission lease, retry floor with jitter, and browser lifecycle policy.
//! Zave's correctness work explains why fingers remain an optimization rather
//! than a ring-safety premise: <https://arxiv.org/abs/1502.06461>.
//!
//! # Algorithm flow
//!
//! ```text
//! request slot i + authenticated successor s
//!        |
//!        v
//! validate i < configured slot count
//!        |
//!        +--> false ----------------------------> reject
//!        |
//!        v
//! s == local?
//!        | yes                                  | no
//!        v                                      v
//! prove i..last                      distance d = clockwise(local, s)
//!                                               |
//!                                               v
//!                                   d >= 2^i ?
//!                                      | no          | yes
//!                                      v             v
//!                                    reject   end = floor(log2(d))
//!                                                    |
//!                                                    v
//!                                          clamp end to last slot
//! ```

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
    /// Zero-based lower slot requested by this lookup.
    ///
    /// The `u16` representation is the serialized protocol form; construction
    /// rejects local indices that cannot be represented without truncation.
    pub(crate) slot: u16,
    /// Unique correlation value for this lookup attempt.
    ///
    /// The state machine requires an exact UUID match so delayed reports for an
    /// earlier lookup of the same slot cannot consume current ownership.
    pub(crate) request_id: uuid::Uuid,
}

impl FingerFixRequest {
    /// Build a request when `slot` can be represented on the wire.
    ///
    /// Returns `None` rather than truncating when `slot` exceeds `u16`. The
    /// supplied UUID is preserved verbatim as the attempt correlation token.
    pub(crate) fn new(slot: usize, request_id: uuid::Uuid) -> Option<Self> {
        Some(Self {
            slot: u16::try_from(slot).ok()?,
            request_id,
        })
    }

    /// Return the lowest finger slot whose successor this request asks to prove.
    ///
    /// The value remains in its serialized `u16` representation; local vector
    /// indexing should use the checked construction-backed `Self::slot_index`.
    pub const fn slot(self) -> u16 {
        self.slot
    }

    /// Return the fresh UUID allocated for this lookup.
    ///
    /// Consumers echo this token in reports and cancellations to establish
    /// exact ownership of the active attempt.
    pub const fn request_id(self) -> uuid::Uuid {
        self.request_id
    }

    /// Return the zero-based slot index used by local vectors.
    ///
    /// Conversion from `u16` to `usize` is lossless on every Rust target and
    /// cannot produce an index different from the serialized request slot.
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
    /// Return whether this rejection should increase retry pressure.
    ///
    /// Invalid and expired current attempts are network failures. Stale reports
    /// are harmless reorderings and must not delay a newer attempt.
    pub(super) const fn counts_as_failure(self) -> bool {
        matches!(self, Self::Invalid | Self::Expired)
    }
}

/// Outcome of committing a report to the finger table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerApplyOutcome {
    /// At least one still-current slot in this proved range was committed.
    Applied,
    /// The report was rejected without changing a finger hint.
    Rejected(
        /// Exact validation reason; the caller uses it to distinguish retryable
        /// current failure from harmless stale reordering.
        FingerReportRejection,
    ),
}

/// Outcome of retaining a report for transport admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerDeferOutcome {
    /// The timely proof is owned by the admission lease.
    Deferred,
    /// The report was rejected and no admission work should start.
    Rejected(
        /// Exact validation reason explaining why no admission lease was created.
        FingerReportRejection,
    ),
}

/// Outcome of retiring a reported candidate that transport cannot use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FingerRetireOutcome {
    /// The exact, still-current report was retired into retry backoff.
    Retired,
    /// Candidate validation failed. A stale conflicting report is left without
    /// effect; an invalid or expired current report retires into backoff.
    Rejected(
        /// Exact validation reason; stale conflicts preserve ownership, while
        /// invalid or expired current reports retire into retry backoff.
        FingerReportRejection,
    ),
}

/// Validated range carried between the attempt and evidence layers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct FingerRangeProof {
    /// Original lookup token at the lower end of the proved range.
    ///
    /// Its slot is the inclusive lower bound and its UUID identifies the exact
    /// attempt that produced this proof.
    pub(super) request: FingerFixRequest,
    /// Evidence epoch observed when the lookup was issued.
    ///
    /// Slots changed under a later epoch are skipped during application so the
    /// proof cannot roll back newer topology information.
    pub(super) issued_epoch: u64,
    /// Authenticated successor returned for the lookup target.
    ///
    /// The same successor is applied across every still-current covered slot;
    /// the local node is represented as an absent remote hint.
    pub(super) successor: Did,
    /// Inclusive upper slot proved by the same successor.
    ///
    /// Validation guarantees this is at least the request slot and below the
    /// configured table width before the proof enters production state.
    pub(super) end: usize,
}

impl FingerRangeProof {
    /// Number of consecutive slots carried by this proof.
    ///
    /// Checked subtraction makes restored local state fail closed if its range
    /// is ever malformed; a valid proof always has `end >= request.slot`.
    /// The returned count is inclusive of both range endpoints and is zero for
    /// a malformed reversed range.
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
///
/// Returns `None` when the start slot is outside the table, the successor lies
/// before the first requested target, or an integer conversion cannot preserve
/// the computed upper bound. Returning the local node proves the complete tail.
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
/// Unit tests for the Chord range-bound lemma and its rejection boundaries.
mod tests {
    use super::*;

    /// Verify the range-bound lemma at exact power-of-two boundaries.
    ///
    /// The cases witness an interior distance, an exact power of two, ring
    /// wraparound to the local node, an invalid predecessor, and an out-of-range
    /// slot. Together they pin both inclusive range semantics and rejection.
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
