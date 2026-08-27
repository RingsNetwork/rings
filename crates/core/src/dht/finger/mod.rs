//! FingerTable

#![deny(missing_docs)]

use serde::Deserialize;
use serde::Serialize;

use crate::dht::did::BiasId;
use crate::dht::Did;

/// Default number of Chord finger slots for a 160-bit `Did`.
pub const DEFAULT_FINGER_TABLE_SIZE: usize = 160;

/// Finger table of Chord DHT
/// Ring's finger table is implemented with BiasRing
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FingerTable {
    did: Did,
    size: usize,
    finger: Vec<Option<Did>>,
    pub(super) fix_finger_index: usize,
}

impl PartialEq for FingerTable {
    fn eq(&self, other: &Self) -> bool {
        self.did == other.did && self.size == other.size && self.finger == other.finger
    }
}

impl Eq for FingerTable {}

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

    fn write_slot(&mut self, index: usize, did: Option<Did>) {
        if let Some(slot) = self.finger.get_mut(index) {
            *slot = did;
        }
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
        self.write_slot(index, Some(did));
    }

    /// setter for fix_finger_index
    pub fn set_fix(&mut self, did: Did) {
        let index = self.fix_finger_index;
        self.set(index, did)
    }

    /// remove a node from dht finger table
    pub fn remove(&mut self, did: Did) {
        self.finger = crate::dht::topology::remove_finger_peer(&self.finger, did);
    }

    /// Join FingerTable
    pub fn join(&mut self, did: Did) {
        let observer = self.did;
        let bias = did.bias(observer);

        for k in 0..self.size {
            let pos = Did::power_of_two(k);

            if bias.pos() < pos {
                continue;
            }

            if let Some(v) = self.finger.get(k).copied().flatten() {
                if BiasId::cmp_from_observer(observer, did, v) == std::cmp::Ordering::Greater {
                    continue;
                }
            }

            self.write_slot(k, Some(did));
        }
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

    /// get finger list
    pub fn list(&self) -> &Vec<Option<Did>> {
        &self.finger
    }

    /// Replace the full finger state with a value produced by the pure topology transition.
    ///
    /// Post: the table keeps its fixed slot count; entries beyond that count
    /// are ignored, missing entries become `None`, and the fix cursor is
    /// clamped to a valid slot when the table is non-empty.
    pub(crate) fn replace_state(&mut self, fingers: &[Option<Did>], fix_finger_index: usize) {
        self.finger = fingers.iter().copied().take(self.size).collect();
        self.finger.resize(self.size, None);
        self.fix_finger_index = if self.size == 0 {
            0
        } else {
            fix_finger_index % self.size
        };
    }

    /// Reset finger table to empty vector
    #[cfg(test)]
    pub fn reset_finger(&mut self) {
        self.finger = vec![None; self.size]
    }

    /// Clone a finger table
    #[cfg(test)]
    pub fn clone_finger(self) -> Vec<Option<Did>> {
        self.finger
    }
}

#[cfg(test)]
mod tests;
