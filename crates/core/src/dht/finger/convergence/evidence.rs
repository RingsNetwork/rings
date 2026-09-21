//! Versioned evidence for each finger slot.
//!
//! A hint and evidence are different facts. `FingerTable` may infer a useful
//! peer from a join before a lookup proves that peer is the true successor of
//! the slot target. Each slot therefore stores both a verification bit and the
//! topology epoch in which its hint last changed.
//!
//! Invariant: `slots.len()` is the finger-table width. Keeping epoch and
//! verification in one `FingerSlotEvidence` prevents the parallel-vector drift
//! that would otherwise make an epoch describe the wrong slot.
//!
//! Epoch versioning is a Rings engineering mechanism, not part of the Chord
//! paper. Its proof obligation is local: a result issued at epoch `e` may write
//! slot `i` only when `slots[i].changed_at <= e`. This is the same comparison in
//! both the preflight check and the committing loop below.
//!
//! # Algorithm flow
//!
//! ```text
//! Event A: a local topology transition changes inferred hints
//!
//! old hints + new hints -> compare tracked slots
//!        | unchanged                    | changed
//!        v                              v
//! keep epoch and verification      increment evidence epoch
//!                                             |
//!                                +------------+------------+
//!                                | available               | overflow
//!                                v                         v
//!                     stamp changed slots          unverify every slot
//!
//! Event B: a later scheduler/network round obtains fresh evidence
//!
//! issue lookup(slot, UUID, current epoch)
//!                  |
//!                  v
//! receive authenticated successor report
//!                  |
//!                  v
//! parent validates token + deadline + Chord geometry
//!                  |
//!                  v
//! derive local FingerRangeProof for covered slots
//!                  |
//!                  v
//! for each slot: changed_at <= proof.issued_epoch?
//!                  | yes                         | no
//!                  v                             v
//!          write hint + verify            preserve newer slot
//!                  |
//!                  v
//! more unverified ranges? -- yes --> later scheduler/network round
//!                  |
//!                  no
//!                  v
//!            convergence complete
//! ```

use super::proof::FingerRangeProof;
use crate::dht::Did;

/// Verification state for one finger-table slot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FingerSlotEvidence {
    /// Epoch in which the slot's inferred hint last changed.
    ///
    /// A proof captured before this value cannot update this slot.
    changed_at: u64,
    /// Whether a lookup or equivalent local proof has verified this slot.
    ///
    /// Changing the corresponding hint always resets this bit under a new epoch.
    verified: bool,
}

impl FingerSlotEvidence {
    /// Create unverified evidence tied to a specific hint-change epoch.
    ///
    /// The explicit false bit prevents new or resized slots from inheriting proof.
    const fn unverified(changed_at: u64) -> Self {
        Self {
            changed_at,
            verified: false,
        }
    }

    /// Whether `proof` may write this slot: the hint did not change after the
    /// proof's lookup was issued (`changed_at <= issued_epoch`).
    const fn admits(self, proof: FingerRangeProof) -> bool {
        self.changed_at <= proof.issued_epoch
    }
}

/// Result of advancing evidence after hint changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EvidenceInvalidation {
    /// No tracked hint changed, so evidence remains byte-for-byte equivalent.
    Unchanged,
    /// Changed hints were stamped under a newly allocated evidence epoch.
    Advanced,
    /// The epoch counter is exhausted. Every active proof must be retired.
    ///
    /// All slots become unverified because no larger version can order changes.
    EpochExhausted,
}

/// Versioned evidence for the complete local finger table.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct FingerEvidence {
    /// Monotonic counter bumped whenever one or more hints change.
    ///
    /// This in-memory revision order is unrelated to wall-clock time.
    epoch: u64,
    /// One evidence record per finger slot.
    ///
    /// Its fixed length gives hints, requests, and evidence one index space.
    slots: Vec<FingerSlotEvidence>,
}

impl FingerEvidence {
    /// Create unverified evidence for a table width.
    ///
    /// All `slot_count` records begin at epoch zero with no accepted proof.
    pub(super) fn new(slot_count: usize) -> Self {
        Self {
            epoch: 0,
            slots: vec![FingerSlotEvidence::unverified(0); slot_count],
        }
    }

    /// Return the current hint-change epoch.
    ///
    /// Request issuance captures this value for later freshness comparison.
    pub(super) const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Return true when every slot has current proof evidence.
    ///
    /// The empty table satisfies this predicate vacuously.
    pub(super) fn all_verified(&self) -> bool {
        self.slots.iter().all(|slot| slot.verified)
    }

    /// Return true when no slot has current proof evidence.
    ///
    /// The predicate makes repeated full invalidation an idempotent transition.
    pub(super) fn all_unverified(&self) -> bool {
        self.slots.iter().all(|slot| !slot.verified)
    }

    /// Return whether any slot at or after `first_slot` still needs proof.
    ///
    /// Earlier slots may be covered by stabilization and are deliberately skipped.
    pub(super) fn any_unverified_from(&self, first_slot: usize) -> bool {
        self.slots
            .iter()
            .skip(first_slot)
            .any(|slot| !slot.verified)
    }

    /// Return the first slot at or after `first_slot` that needs proof.
    ///
    /// Search is ascending; `None` means the requested suffix is fully verified.
    pub(super) fn first_unverified_from(&self, first_slot: usize) -> Option<usize> {
        self.first_unverified_in(first_slot..self.slots.len())
    }

    /// Return the first slot in `range` that needs proof.
    fn first_unverified_in(&self, range: std::ops::Range<usize>) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .take(range.end)
            .skip(range.start)
            .find_map(|(index, slot)| (!slot.verified).then_some(index))
    }

    /// Return the next unverified slot in cyclic order from `cursor`, never
    /// selecting a slot below `first_slot`.
    ///
    /// The search covers `[max(cursor, first_slot), len)` and then wraps to
    /// `[first_slot, cursor)`. Resuming after the previous attempt instead of
    /// always at the lowest unverified slot is what keeps one persistently
    /// failing slot from monopolizing the single in-flight attempt: every
    /// unverified slot is selected once per rotation.
    pub(super) fn next_unverified_cyclic(&self, first_slot: usize, cursor: usize) -> Option<usize> {
        let start = cursor.max(first_slot);
        self.first_unverified_from(start)
            .or_else(|| self.first_unverified_in(first_slot..start))
    }

    /// Inclusive end of the run of equal hints that starts at `slot`.
    ///
    /// Slots sharing one inferred hint have one expected successor, so a proof
    /// or a failure for `slot` is evidence about the whole run.
    pub(super) fn hint_run_end(fingers: &[Option<Did>], slot: usize) -> usize {
        let Some(value) = fingers.get(slot) else {
            return slot;
        };
        fingers
            .iter()
            .enumerate()
            .skip(slot)
            .take_while(|(_, finger)| *finger == value)
            .map(|(index, _)| index)
            .last()
            .unwrap_or(slot)
    }

    /// Return whether the hint at `slot` changed between two topology snapshots.
    ///
    /// Indexed `get` comparison safely handles unequal snapshot lengths.
    pub(super) fn hint_changed_at(
        before: &[Option<Did>],
        after: &[Option<Did>],
        slot: usize,
    ) -> bool {
        before.get(slot) != after.get(slot)
    }

    /// Invalidate precisely the slots whose inferred hint changed.
    ///
    /// Preservation: advancing `epoch` before marking changed slots ensures a
    /// proof issued in an older epoch cannot overwrite those slots. If the
    /// counter is exhausted, every slot is conservatively unverified and the
    /// caller retires the active proof.
    /// The result distinguishes no-op, selective versioning, and global expiry.
    pub(super) fn invalidate_hint_changes(
        &mut self,
        before: &[Option<Did>],
        after: &[Option<Did>],
    ) -> EvidenceInvalidation {
        // Hints and evidence share the table width fixed at construction;
        // every tracked slot is compared within that common index space.
        let has_change =
            (0..self.slots.len()).any(|index| Self::hint_changed_at(before, after, index));
        if !has_change {
            return EvidenceInvalidation::Unchanged;
        }

        let Some(next_epoch) = self.epoch.checked_add(1) else {
            self.slots.iter_mut().for_each(|slot| slot.verified = false);
            return EvidenceInvalidation::EpochExhausted;
        };
        self.epoch = next_epoch;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if Self::hint_changed_at(before, after, index) {
                *slot = FingerSlotEvidence::unverified(next_epoch);
            }
        }
        EvidenceInvalidation::Advanced
    }

    /// Mark every slot unverified after a discontinuity in membership view.
    ///
    /// A fresh epoch is stamped when available. On overflow, epochs remain but
    /// all verification bits clear so stale evidence is never presented as current.
    pub(super) fn invalidate_all(&mut self) {
        if let Some(next_epoch) = self.epoch.checked_add(1) {
            self.epoch = next_epoch;
            self.slots.fill(FingerSlotEvidence::unverified(next_epoch));
        } else {
            self.slots.iter_mut().for_each(|slot| slot.verified = false);
        }
    }

    /// Reopen the consecutive hint range at `cursor` for periodic revalidation.
    ///
    /// The run starting at `cursor` (wrapping once past the table end) is
    /// cleared; only slots sharing its first hint value are affected. Empty
    /// tables remain unchanged.
    pub(super) fn reopen_next_range(&mut self, fingers: &[Option<Did>], cursor: usize) {
        let slot_count = fingers.len();
        if slot_count == 0 {
            return;
        }
        let start = cursor % slot_count;
        // The run may consist of `None` hints: consecutive empty hints also need
        // periodic revalidation because absence is only local inferred state.
        let end = Self::hint_run_end(fingers, start);
        for evidence in self
            .slots
            .iter_mut()
            .take(end.saturating_add(1))
            .skip(start)
        {
            evidence.verified = false;
        }
    }

    /// Check that a proof can still update at least one slot in its range.
    ///
    /// One slot whose change epoch is no newer than the issue epoch is enough to
    /// admit a partial, non-rollback commit. This is the same per-slot
    /// predicate that [`Self::apply`] commits by, so an accepted proof always
    /// commits at least one slot.
    pub(super) fn accepts(&self, proof: FingerRangeProof) -> bool {
        self.slots
            .iter()
            .skip(proof.request.slot_index())
            .take(proof.covered_slot_count())
            .any(|slot| slot.admits(proof))
    }

    /// Apply a validated range without overwriting evidence from a newer epoch.
    ///
    /// Post: every slot in the range that admits the proof contains the
    /// reported successor and is marked verified. A newer slot is skipped
    /// rather than rolled back.
    pub(super) fn apply(
        &mut self,
        fingers: &mut [Option<Did>],
        local: Did,
        proof: FingerRangeProof,
    ) {
        // A self-successor proof means this node owns the target range; the
        // table stores that as no remote finger hint.
        let replacement = (proof.successor != local).then_some(proof.successor);
        for (finger, evidence) in fingers
            .iter_mut()
            .zip(&mut self.slots)
            .skip(proof.request.slot_index())
            .take(proof.covered_slot_count())
        {
            if evidence.admits(proof) {
                *finger = replacement;
                evidence.verified = true;
            }
        }
    }

    /// Confirm a range using evidence obtained outside finger lookup traffic.
    ///
    /// The inclusive range is clamped to width; invalid ranges are no-ops. The
    /// result reports whether any slot changed to verified.
    pub(super) fn confirm_range(&mut self, start: usize, end: usize) -> bool {
        if start > end || start >= self.slots.len() {
            return false;
        }
        let end = end.min(self.slots.len().saturating_sub(1));
        let mut changed = false;
        for slot in self
            .slots
            .iter_mut()
            .skip(start)
            .take(end.saturating_sub(start).saturating_add(1))
        {
            changed |= !slot.verified;
            slot.verified = true;
        }
        changed
    }

    /// Return verification bits for state-machine assertions.
    ///
    /// Values are copied in slot order without exposing epochs or mutation.
    #[cfg(test)]
    pub(super) fn verified(&self) -> Vec<bool> {
        self.slots.iter().map(|slot| slot.verified).collect()
    }

    /// Replace verification bits from the front of the table for tests.
    ///
    /// The full table clears first, then the overlapping prefix is copied;
    /// excess input is ignored and excess slots remain false.
    #[cfg(test)]
    pub(super) fn set_verified(&mut self, values: &[bool]) {
        self.fill_verified(false);
        for (slot, verified) in self.slots.iter_mut().zip(values) {
            slot.verified = *verified;
        }
    }

    /// Set every verification bit for tests.
    ///
    /// Hint-change epochs remain intact so fixtures vary only convergence progress.
    #[cfg(test)]
    pub(super) fn fill_verified(&mut self, verified: bool) {
        self.slots
            .iter_mut()
            .for_each(|slot| slot.verified = verified);
    }

    /// Set one verification bit for tests, returning false if out of range.
    ///
    /// A valid index updates one slot; an invalid index leaves state unchanged.
    #[cfg(test)]
    pub(super) fn set_slot_verified(&mut self, slot: usize, verified: bool) -> bool {
        let Some(slot) = self.slots.get_mut(slot) else {
            return false;
        };
        slot.verified = verified;
        true
    }
}
