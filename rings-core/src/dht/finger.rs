#![warn(missing_docs)]
use super::did::BiasId;
use crate::dht::Did;
use crate::err::Result;
use num_bigint::BigUint;
use serde::Deserialize;
use serde::Serialize;
use std::ops::Index;

/// Finger table of Chord DHT
/// Ring's finger table is implemented with BiasRing
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FingerTable {
    id: Did,
    size: usize,
    finger: Vec<Option<Did>>,
}

impl FingerTable {
    /// builder
    pub fn new(id: Did, size: usize) -> Self {
        Self {
            id,
            size,
            finger: vec![None; size],
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
        &self.finger[index]
    }

    /// setter
    pub fn set(&mut self, index: usize, id: &Did) {
        self.finger[index] = Some(*id);
    }

    /// remove a node from dht finger table
    /// remote a node from dht successor table
    /// if suuccessor is empty, set it to the cloest node
    pub fn remove(&mut self, id: Did) {
        self.finger.retain(|v| *v == Some(id));
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
