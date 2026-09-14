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

use serde::Deserialize;
use serde::Serialize;

use super::proof::FingerRangeProof;
use crate::dht::Did;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct FingerSlotEvidence {
    changed_at: u64,
    verified: bool,
}

impl FingerSlotEvidence {
    const fn unverified(changed_at: u64) -> Self {
        Self {
            changed_at,
            verified: false,
        }
    }
}

/// Result of advancing evidence after hint changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EvidenceInvalidation {
    Unchanged,
    Advanced,
    /// The epoch counter is exhausted. Every active proof must be retired.
    EpochExhausted,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(super) struct FingerEvidence {
    epoch: u64,
    slots: Vec<FingerSlotEvidence>,
}

impl FingerEvidence {
    pub(super) fn new(slot_count: usize) -> Self {
        Self {
            epoch: 0,
            slots: vec![FingerSlotEvidence::unverified(0); slot_count],
        }
    }

    pub(super) fn normalize(&mut self, slot_count: usize) {
        self.slots
            .resize(slot_count, FingerSlotEvidence::unverified(self.epoch));
    }

    pub(super) const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(super) fn all_verified(&self) -> bool {
        self.slots.iter().all(|slot| slot.verified)
    }

    pub(super) fn all_unverified(&self) -> bool {
        self.slots.iter().all(|slot| !slot.verified)
    }

    pub(super) fn any_unverified_from(&self, first_slot: usize) -> bool {
        self.slots
            .iter()
            .skip(first_slot)
            .any(|slot| !slot.verified)
    }

    pub(super) fn first_unverified_from(&self, first_slot: usize) -> Option<usize> {
        self.slots
            .iter()
            .enumerate()
            .skip(first_slot)
            .find_map(|(index, slot)| (!slot.verified).then_some(index))
    }

    /// Whether the hint at `slot` changed between two topology snapshots.
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
    pub(super) fn invalidate_hint_changes(
        &mut self,
        before: &[Option<Did>],
        after: &[Option<Did>],
    ) -> EvidenceInvalidation {
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

    pub(super) fn invalidate_all(&mut self) {
        if let Some(next_epoch) = self.epoch.checked_add(1) {
            self.epoch = next_epoch;
            self.slots.fill(FingerSlotEvidence::unverified(next_epoch));
        } else {
            self.slots.iter_mut().for_each(|slot| slot.verified = false);
        }
    }

    /// Reopen the next consecutive hint range after a completed pass.
    pub(super) fn reopen_next_range(&mut self, fingers: &[Option<Did>], cursor: usize) {
        let slot_count = fingers.len();
        if slot_count == 0 {
            return;
        }
        let start = cursor.saturating_add(1) % slot_count;
        let Some(value) = fingers.get(start).copied() else {
            return;
        };
        for (finger, evidence) in fingers
            .iter()
            .skip(start)
            .zip(self.slots.iter_mut().skip(start))
        {
            if *finger != value {
                break;
            }
            evidence.verified = false;
        }
    }

    /// Check that a proof can still update at least one slot in its range.
    pub(super) fn accepts(&self, proof: FingerRangeProof) -> bool {
        self.slots
            .iter()
            .skip(proof.request.slot_index())
            .take(proof.covered_slot_count())
            .any(|slot| slot.changed_at <= proof.issued_epoch)
    }

    /// Apply a validated range without overwriting evidence from a newer epoch.
    ///
    /// Post: every eligible slot in the range contains the reported successor
    /// and is marked verified. A newer slot is skipped rather than rolled back.
    pub(super) fn apply(
        &mut self,
        fingers: &mut [Option<Did>],
        local: Did,
        proof: FingerRangeProof,
    ) -> bool {
        let replacement = (proof.successor != local).then_some(proof.successor);
        let mut applied = false;
        for (finger, evidence) in fingers
            .iter_mut()
            .zip(&mut self.slots)
            .skip(proof.request.slot_index())
            .take(proof.covered_slot_count())
        {
            if evidence.changed_at <= proof.issued_epoch {
                *finger = replacement;
                evidence.verified = true;
                applied = true;
            }
        }
        applied
    }

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

    #[cfg(test)]
    pub(super) fn verified(&self) -> Vec<bool> {
        self.slots.iter().map(|slot| slot.verified).collect()
    }

    #[cfg(test)]
    pub(super) fn set_verified(&mut self, values: &[bool]) {
        self.fill_verified(false);
        for (slot, verified) in self.slots.iter_mut().zip(values) {
            slot.verified = *verified;
        }
    }

    #[cfg(test)]
    pub(super) fn fill_verified(&mut self, verified: bool) {
        self.slots
            .iter_mut()
            .for_each(|slot| slot.verified = verified);
    }

    #[cfg(test)]
    pub(super) fn set_slot_verified(&mut self, slot: usize, verified: bool) -> bool {
        let Some(slot) = self.slots.get_mut(slot) else {
            return false;
        };
        slot.verified = verified;
        true
    }
}
