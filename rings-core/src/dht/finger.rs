#![warn(missing_docs)]
use std::ops::Index;

use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;

use super::did::BiasId;
use crate::dht::Did;
use crate::err::Result;

/// Finger table of Chord DHT
/// Ring's finger table is implemented with BiasRing
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerTable {
    id: Did,
    size: usize,
    finger: Vec<Option<Did>>,
    pub(super) fix_finger_index: u8,
}

impl FingerTable {
    /// builder
    pub fn new(id: Did, size: usize) -> Self {
        Self {
            id,
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
        let ids = self
            .finger
            .iter()
            .filter(|x| x.is_some())
            .take(1)
            .map(|x| x.unwrap())
            .collect::<Vec<Did>>();
        ids.first().copied()
    }

    /// getter
    pub fn get(&self, index: usize) -> &Option<Did> {
        if index >= self.finger.len() {
            return &None;
        }
        &self.finger[index]
    }

    /// setter
    pub fn set(&mut self, index: usize, id: &Did) {
        if index >= self.finger.len() {
            return;
        }
        self.finger[index] = Some(*id);
    }

    /// setter for fix_finger_index
    pub fn set_fix(&mut self, id: &Did) {
        let index = self.fix_finger_index as usize;
        self.set(index, id)
    }

    /// remove a node from dht finger table
    pub fn remove(&mut self, id: Did) {
        for (index, item) in self.finger.clone().iter().enumerate() {
            if *item == Some(id) {
                self.finger[index] = None;
            }
        }
    }

    /// Join FingerTable
    pub fn join(&mut self, id: Did) {
        let bid: BiasId = id.bias(&self.id);

        for k in 0u32..self.size as u32 {
            // (n + 2^k) % 2^m >= n
            // pos >= id
            // from n to n + 2^160
            let pos = Did::from(BigUint::from(2u16).pow(k));
            // pos less than id
            if bid.pos() >= pos {
                // if pos <= id - self.id {
                match self.finger[k as usize] {
                    Some(v) => {
                        // for a existed value v
                        // if id is more close to self.id than v
                        if bid < v.bias(&self.id) {
                            // if id < v || id > -v {
                            self.finger[k as usize] = Some(id);
                            // if id is more close to successor
                        }
                    }
                    None => {
                        self.finger[k as usize] = Some(id);
                    }
                }
            }
        }
    }

    /// Check finger is contains some node
    pub fn contains(&self, v: &Option<Did>) -> bool {
        self.finger.contains(v)
    }

    /// closest_preceding_node
    pub fn closest(&self, id: Did) -> Result<Did> {
        let bid: BiasId = id.bias(&self.id);
        for i in (0..self.size).rev() {
            if let Some(v) = self.finger[i as usize] {
                if v.bias(&self.id) < bid {
                    // after bias v > self.id
                    // check a recorded did x in (self.id, target_id)
                    return Ok(v);
                }
            }
        }
        Ok(self.id)
    }

    /// get length of finger
    pub fn len(&self) -> usize {
        self.finger.iter().flatten().count() as usize
    }

    /// get finger list
    pub fn list(&self) -> &Vec<Option<Did>> {
        &self.finger
    }
}

impl Index<usize> for FingerTable {
    type Output = Option<Did>;
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ecc::SecretKey;

    #[test]
    fn test_finger_table_get_set_remove() {
        let key = SecretKey::random();
        let id: Did = key.address().into();
        let mut table = FingerTable::new(id, 3);
        println!("check finger len");
        assert_eq!(table.len(), 0);
        assert_eq!(table.finger.len(), 3);
        println!("check finger all items is none");
        assert!(*table.get(0) == None, "index 0 should be None");
        assert!(*table.get(1) == None, "index 1 should be None");
        assert!(*table.get(2) == None, "index 2 should be None");
        assert!(*table.get(3) == None, "index 3 should be None");

        println!("set finger item");
        let id1: Did = SecretKey::random().address().into();
        let id2: Did = SecretKey::random().address().into();
        let id3: Did = SecretKey::random().address().into();
        let id4: Did = SecretKey::random().address().into();

        table.set(0, &id1);
        assert_eq!(table.len(), 1);
        assert!(
            *table.get(0) == Some(id1),
            "expect value at index 0 is {:?}, got {:?}",
            Some(id1),
            table.get(0)
        );
        // can not be set, because size is 2
        table.set(2, &id3);
        assert_eq!(table.len(), 2);
        assert_eq!(table.finger.len(), 3);
        assert!(
            *table.get(1) == None,
            "expect value at index 1 is None, got {:?}",
            table.get(1)
        );
        assert!(
            *table.get(2) == Some(id3),
            "expect value at index 2 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        println!("set value out of index");
        table.set(4, &id4);
        assert_eq!(table.len(), 2);
        assert_eq!(table.finger.len(), 3);

        println!("remove node from finger");
        table.remove(id1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.finger.len(), 3);
        assert!(
            *table.get(0) == None,
            "expect value at index 1 is None, got {:?}",
            table.get(0)
        );
        assert!(
            *table.get(2) == Some(id3),
            "expect value at index 2 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        table.set(0, &id1);
        table.set(1, &id2);

        assert!(
            *table.get(0) == Some(id1),
            "expect value at index 0 is {:?}, got {:?}",
            Some(id1),
            table.get(0)
        );
        assert!(
            *table.get(1) == Some(id2),
            "expect value at index 1 is {:?}, got {:?}",
            Some(id2),
            table.get(1)
        );
        assert!(
            *table.get(2) == Some(id3),
            "expect value at index 1 is {:?}, got {:?}",
            Some(id3),
            table.get(2)
        );

        assert!(
            *table.get(3) == None,
            "expect value at index 2 is None, got {:?}",
            table.get(3)
        );

        table.remove(id1);
        assert_eq!(table.len(), 2);
        assert_eq!(table.finger.len(), 3);
        let t1 = table.get(0);
        assert!(*t1 == None, "expect value at index 0 is None, got {:?}", t1);
        let t2 = table.get(1);
        assert!(
            *t2 == Some(id2),
            "expect value at index 1 is {:?}, got {:?}",
            Some(id2),
            t2,
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
}
