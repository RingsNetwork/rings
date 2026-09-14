//! FingerTable

#![deny(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use crate::dht::did::BiasId;
use crate::dht::Did;

/// Range-aware convergence state machine for this finger table.
mod convergence;

pub(crate) use convergence::finger_lookup_backoff_ms;
pub(crate) use convergence::finger_proof_end;
pub(crate) use convergence::FingerApplyOutcome;
pub(crate) use convergence::FingerConvergencePhase;
#[cfg(test)]
pub(crate) use convergence::FingerConvergenceProjection;
pub(crate) use convergence::FingerConvergenceState;
pub(crate) use convergence::FingerConvergenceStatus;
pub(crate) use convergence::FingerDeferOutcome;
pub use convergence::FingerFixRequest;
pub(crate) use convergence::FingerReportRejection;
pub(crate) use convergence::FingerRetireOutcome;
pub(crate) use convergence::FINGER_ADMISSION_TIMEOUT_MS;
#[cfg(test)]
pub(crate) use convergence::FINGER_LOOKUP_MIN_INTERVAL_MS;

/// Default number of Chord finger slots for a 160-bit `Did`.
pub const DEFAULT_FINGER_TABLE_SIZE: usize = 160;

/// Finger table of the Rings Chord DHT.
///
/// Equality compares the complete serializable protocol state, including the
/// maintenance cursor, convergence ownership, evidence epochs, and retry
/// state. Call [`Self::list`] when only routing hints should be compared.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FingerTable {
    /// Local node whose outgoing fingers this table describes.
    did: Did,
    /// Fixed number of address-space slots maintained by this table.
    size: usize,
    /// Current inferred routing hint for each slot; `None` can mean self or unknown.
    finger: Vec<Option<Did>>,
    /// Cursor used by periodic maintenance to resume range revalidation.
    pub(super) fix_finger_index: usize,
    /// Verification and retry state attached to the inferred hints.
    ///
    /// This serialized state binds every hint to freshness evidence, owns at
    /// most one maintenance request, and preserves retry pacing across topology
    /// transitions.
    convergence: FingerConvergenceState,
}

impl FingerTable {
    /// builder
    ///
    /// `Did` is represented by H160, so finger slots above 160 would wrap the
    /// `2^index` lookup target back into the same 160-bit space. Values above
    /// [`DEFAULT_FINGER_TABLE_SIZE`] are clamped; zero is allowed for tests that
    /// intentionally disable finger maintenance.
    pub fn new(did: Did, size: usize) -> Self {
        let size = size.min(DEFAULT_FINGER_TABLE_SIZE);
        Self {
            did,
            size,
            finger: vec![None; size],
            fix_finger_index: 0,
            convergence: FingerConvergenceState::new(size),
        }
    }

    /// is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get first element from Finger Table
    pub fn first(&self) -> Option<Did> {
        self.finger.iter().flatten().next().copied()
    }

    /// getter
    pub fn get(&self, index: usize) -> Option<Did> {
        self.finger.get(index).copied().flatten()
    }

    /// Apply a hint mutation and invalidate only evidence touched by it.
    ///
    /// The complete pre-mutation hint vector is retained long enough to compare
    /// each slot after `mutation` runs. The convergence state then advances only
    /// evidence whose corresponding hint changed and retires ownership when its
    /// proof premise was invalidated.
    fn mutate_hints(&mut self, mutation: impl FnOnce(&mut Vec<Option<Did>>)) {
        let before = self.finger.clone();
        mutation(&mut self.finger);
        self.convergence
            .invalidate_hint_changes(&before, &self.finger);
    }

    /// setter
    pub fn set(&mut self, index: usize, did: Did) {
        tracing::debug!("set finger table index: {} did: {}", index, did);
        if index >= self.finger.len() {
            tracing::error!("set finger index out of range, index: {}", index);
            return;
        }
        if did == self.did {
            tracing::trace!("set finger table with self did, ignore it");
            return;
        }
        self.mutate_hints(|fingers| {
            if let Some(slot) = fingers.get_mut(index) {
                *slot = Some(did);
            }
        });
    }

    /// setter for fix_finger_index
    pub fn set_fix(&mut self, did: Did) {
        let index = self.fix_finger_index;
        self.set(index, did)
    }

    /// remove a node from dht finger table
    pub fn remove(&mut self, did: Did) {
        self.mutate_hints(|fingers| {
            *fingers = crate::dht::topology::remove_finger_peer(fingers, did);
        });
    }

    /// Join FingerTable
    pub fn join(&mut self, did: Did) {
        let observer = self.did;
        // `bias` is the candidate's clockwise distance from this table owner;
        // it determines the lowest power-of-two slot the candidate can cover.
        let bias = did.bias(observer);
        let size = self.size;

        self.mutate_hints(|fingers| {
            for k in 0..size {
                let pos = Did::power_of_two(k);

                if bias.pos() < pos {
                    continue;
                }

                if let Some(v) = fingers.get(k).copied().flatten() {
                    if BiasId::cmp_from_observer(observer, did, v) == std::cmp::Ordering::Greater {
                        continue;
                    }
                }

                if let Some(slot) = fingers.get_mut(k) {
                    *slot = Some(did);
                }
            }
        });
    }

    /// Check finger is contains some node
    pub fn contains(&self, v: Option<Did>) -> bool {
        self.finger.contains(&v)
    }

    /// get closest predecessor
    pub fn closest_predecessor(&self, did: Did) -> Did {
        let observer = self.did;

        for i in (0..self.size).rev() {
            if let Some(v) = self.finger.get(i).copied().flatten() {
                if BiasId::cmp_from_observer(observer, v, did) == std::cmp::Ordering::Less {
                    return v;
                }
            }
        }

        self.did
    }

    /// get length of finger
    pub fn len(&self) -> usize {
        self.finger.iter().flatten().count()
    }

    /// Get the number of slots in this finger table.
    pub fn slot_count(&self) -> usize {
        self.size
    }

    /// Get the next finger index maintained by the periodic fixer.
    pub fn fix_finger_index(&self) -> usize {
        self.fix_finger_index
    }

    /// Borrow the convergence state associated with these finger hints.
    ///
    /// The immutable borrow lets topology and scheduling code inspect evidence,
    /// ownership, and retry projections without bypassing `FingerTable`'s hint
    /// mutation boundary.
    pub(crate) fn convergence_state(&self) -> &FingerConvergenceState {
        &self.convergence
    }

    /// Prepare an exact slot request through the real convergence path.
    ///
    /// Test fixtures use a fixed monotonic timestamp and fresh UUID while the
    /// production state machine handles verification flags, attempt ownership,
    /// and pacing. An invalid slot returns `None` without creating work.
    #[cfg(all(test, feature = "dummy", not(target_family = "wasm")))]
    pub(crate) fn prepare_request_for_test(
        &mut self,
        slot: usize,
    ) -> Option<crate::dht::FingerFixRequest> {
        self.convergence
            .prepare_slot_for_test(&self.finger, slot, 1_000, crate::utils::new_uuid())
    }

    /// get finger list
    pub fn list(&self) -> &Vec<Option<Did>> {
        &self.finger
    }

    /// Replace the full finger state with a value produced by the pure topology transition.
    ///
    /// Post: the table keeps its fixed slot count; entries beyond that count
    /// are ignored, missing entries become `None`, and the fix cursor is
    /// clamped to a valid slot when the table is non-empty. Convergence state
    /// is normalized to the same width so restored proofs cannot address a
    /// slot that no longer exists.
    pub(crate) fn replace_state(
        &mut self,
        fingers: &[Option<Did>],
        fix_finger_index: usize,
        convergence: FingerConvergenceState,
    ) {
        self.finger = fingers.iter().copied().take(self.size).collect();
        self.finger.resize(self.size, None);
        self.fix_finger_index = if self.size == 0 {
            0
        } else {
            fix_finger_index % self.size
        };
        self.convergence = convergence.normalized(self.size);
    }

    /// Reset finger table to empty vector
    #[cfg(test)]
    pub fn reset_finger(&mut self) {
        self.finger = vec![None; self.size];
        self.convergence = FingerConvergenceState::new(self.size);
    }

    /// Clone a finger table
    #[cfg(test)]
    pub fn clone_finger(self) -> Vec<Option<Did>> {
        self.finger
    }
}

#[cfg(test)]
mod test_finger;
