//! FingerTable

#![warn(missing_docs)]

use derivative::Derivative;
use serde::Deserialize;
use serde::Serialize;

use crate::dht::Did;

/// Default number of Chord finger slots for a 160-bit `Did`.
pub const DEFAULT_FINGER_TABLE_SIZE: usize = 160;

/// Finger table of Chord DHT
/// Ring's finger table is implemented with BiasRing
#[derive(Derivative, Clone, Debug, Serialize, Deserialize)]
#[derivative(PartialEq)]
pub struct FingerTable {
    did: Did,
    size: usize,
    finger: Vec<Option<Did>>,
    #[derivative(PartialEq = "ignore")]
    pub(super) fix_finger_index: usize,
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
        let indexes: Vec<usize> = self
            .finger
            .iter()
            .enumerate()
            .filter(|(_, &x)| x == Some(did))
            .map(|(id, _)| id)
            .collect();

        if let (Some(first_idx), Some(last_idx)) =
            (indexes.first().copied(), indexes.last().copied())
        {
            let end_idx = last_idx + 1;

            // Update to the next did of last equaled did in finger table.
            // If cannot get that, use None.
            let fix_id = self.finger.get(end_idx).copied().flatten();

            for idx in first_idx..end_idx {
                self.write_slot(idx, fix_id);
            }
        }
    }

    /// Join FingerTable
    pub fn join(&mut self, did: Did) {
        let bias = did.bias(self.did);

        for k in 0..self.size {
            let pos = Did::power_of_two(k);

            if bias.pos() < pos {
                continue;
            }

            if let Some(v) = self.finger.get(k).copied().flatten() {
                if bias > v.bias(self.did) {
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
        let bias = did.bias(self.did);

        for i in (0..self.size).rev() {
            if let Some(v) = self.finger.get(i).copied().flatten() {
                if v.bias(self.did) < bias {
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

    /// get finger list
    pub fn list(&self) -> &Vec<Option<Did>> {
        &self.finger
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
mod test {
    use super::*;
    use crate::dht::tests::gen_ordered_dids;

    #[test]
    fn test_finger_table_size_bounds() {
        let did = gen_ordered_dids(1)[0];

        assert_eq!(
            FingerTable::new(did, DEFAULT_FINGER_TABLE_SIZE + 1).slot_count(),
            DEFAULT_FINGER_TABLE_SIZE
        );
        assert_eq!(FingerTable::new(did, 0).slot_count(), 0);
    }

    #[test]
    fn test_finger_table_get_set_remove() {
        let dids = gen_ordered_dids(5);

        let mut table = FingerTable::new(dids[0], 3);
        println!("check finger len");
        assert_eq!(table.len(), 0);
        assert_eq!(table.finger.len(), 3);
        println!("check finger all items is none");
        assert!(table.get(0).is_none(), "index 0 should be None");
        assert!(table.get(1).is_none(), "index 1 should be None");
        assert!(table.get(2).is_none(), "index 2 should be None");
        assert!(table.get(3).is_none(), "index 3 should be None");

        println!("set finger item");
        let (id1, id2, id3, id4) = (dids[1], dids[2], dids[3], dids[4]);

        table.set(0, id1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.finger.len(), 3);

        table.set(2, id3);
        assert_eq!(table.len(), 2);
        assert_eq!(table.finger.len(), 3);

        assert!(
            table.get(0) == Some(id1),
            "expect value at index 0 is {:?}, got {:?}",
            Some(id1),
            table.get(0)
        );
        assert!(
            table.get(1).is_none(),
            "expect value at index 1 is None, got {:?}",
            table.get(1)
        );
        assert!(
            table.get(2) == Some(id3),
            "expect value at index 2 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        println!("set value out of index");
        table.set(4, id4);
        assert_eq!(table.len(), 2);
        assert_eq!(table.finger.len(), 3);

        println!("remove node from finger");
        table.remove(id1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.finger.len(), 3);
        assert!(
            table.get(0).is_none(),
            "expect value at index 1 is None, got {:?}",
            table.get(0)
        );
        assert!(
            table.get(2) == Some(id3),
            "expect value at index 2 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        println!("remove node with auto fill");
        table.set(0, id1);
        table.set(1, id2);
        assert!(
            table.get(0) == Some(id1),
            "expect value at index 0 is {:?}, got {:?}",
            Some(id1),
            table.get(0)
        );
        assert!(
            table.get(1) == Some(id2),
            "expect value at index 1 is {:?}, got {:?}",
            Some(id2),
            table.get(1)
        );
        assert!(
            table.get(2) == Some(id3),
            "expect value at index 2 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        table.remove(id1);
        assert_eq!(table.len(), 3);
        assert_eq!(table.finger.len(), 3);
        assert!(
            table.get(0) == Some(id2),
            "expect value at index 0 is {:?}, got {:?}",
            id2,
            table.get(0)
        );
        assert!(
            table.get(1) == Some(id2),
            "expect value at index 1 is {:?}, got {:?}",
            Some(id2),
            table.get(1),
        );

        println!("remove item not in fingers");
        table.remove(id4);

        println!("remove all items in fingers");
        table.remove(id1);
        assert_eq!(table.first(), Some(id2));

        println!("check first item");
        table.remove(id3);
        assert_eq!(table.first(), Some(id2));

        table.remove(id2);
        assert_eq!(table.first(), None);
        assert_eq!(table.len(), 0);
        assert_eq!(table.finger.len(), 3);
    }

    #[test]
    fn test_finger_table_remove_then_fill() {
        let dids = gen_ordered_dids(6);
        let (did1, did2, did3, did4, did5) = (dids[1], dids[2], dids[3], dids[4], dids[5]);

        let mut table = FingerTable::new(dids[0], 5);

        // [did1, did2, did3, did4, did5] - did1 = [did2, did2, did3, did4, did5]
        table.reset_finger();
        table.set(0, did1);
        table.set(1, did2);
        table.set(2, did3);
        table.set(3, did4);
        table.set(4, did5);
        table.remove(did1);
        assert_eq!(table.finger, [
            Some(did2),
            Some(did2),
            Some(did3),
            Some(did4),
            Some(did5),
        ]);

        // [did1, did2, did3, did4, did5] - did2 = [did1, did3, did3, did4, did5]
        table.reset_finger();
        table.set(0, did1);
        table.set(1, did2);
        table.set(2, did3);
        table.set(3, did4);
        table.set(4, did5);
        table.remove(did2);
        assert_eq!(table.finger, [
            Some(did1),
            Some(did3),
            Some(did3),
            Some(did4),
            Some(did5),
        ]);

        // [did1, None, did3, did4, did5] - did1 = [None, None, did3, did4, did5]
        table.reset_finger();
        table.set(0, did1);
        table.set(2, did3);
        table.set(3, did4);
        table.set(4, did5);
        table.remove(did1);
        assert_eq!(table.finger, [
            None,
            None,
            Some(did3),
            Some(did4),
            Some(did5),
        ]);

        // [did1, None, did3, did4, did5] - did3 = [did1, None, did4, did4, did5]
        table.reset_finger();
        table.set(0, did1);
        table.set(2, did3);
        table.set(3, did4);
        table.set(4, did5);
        table.remove(did3);
        assert_eq!(table.finger, [
            Some(did1),
            None,
            Some(did4),
            Some(did4),
            Some(did5),
        ]);

        // [did1, did2, did3, did4, did5] - did5 = [did1, did2, did4, did4, None]
        table.reset_finger();
        table.set(0, did1);
        table.set(1, did2);
        table.set(2, did3);
        table.set(3, did4);
        table.set(4, did5);
        table.remove(did5);
        assert_eq!(table.finger, [
            Some(did1),
            Some(did2),
            Some(did3),
            Some(did4),
            None
        ]);
    }
}
