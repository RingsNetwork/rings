#![warn(missing_docs)]
//! Successor Sequance for PeerRing
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;

use crate::dht::did::BiasId;
use crate::dht::did::SortRing;
use crate::dht::Did;
use crate::err::Error;
use crate::err::Result;

/// A sequence of successors for a node on the ring.
/// It's necessary to have multiple successors to prevent a single point of failure.
/// Note the successors are in order of a clockwise distance from the node.
/// See also [super::did::BiasId].
#[derive(Debug, Clone)]
pub struct SuccessorSeq {
    /// The identifier of a node
    did: Did,
    /// The maximum number of successors
    max: u8,
    /// The list of successor nodes
    successors: Arc<RwLock<Vec<Did>>>,
}

/// Interface for reading from a `SuccessorSeq`
pub trait SuccessorReader {
    /// Check if the sequence is empty
    fn is_empty(&self) -> Result<bool>;
    /// Check if the sequence is full
    fn is_full(&self) -> Result<bool>;
    /// Retrieve an element at a given index
    fn get(&self, index: usize) -> Result<Did>;
    /// Get the length of the sequence
    fn len(&self) -> Result<usize>;
    /// Get the element with the minimum value
    fn min(&self) -> Result<Did>;
    /// Get the element with the maximum value
    fn max(&self) -> Result<Did>;
    /// Get the list of successors
    fn list(&self) -> Result<Vec<Did>>;
    /// Check if a given identifier exists in the list
    fn contains(&self, did: &Did) -> Result<bool>;
    /// Perform a dry run update
    fn update_dry(&self, did: &[Did]) -> Result<Vec<Did>>;
}

/// Interface for writing to a `SuccessorSeq`
pub trait SuccessorWriter {
    /// Update a successor in the sequence
    fn update(&self, successor: Did) -> Result<Option<Did>>;
    /// Extend the sequence with a list of successors
    fn extend(&self, succ_list: &[Did]) -> Result<Vec<Did>>;
    /// Remove a successor from the sequence
    fn remove(&self, did: Did) -> Result<()>;
}

impl SuccessorSeq {
    /// Constructor for `SuccessorSeq`
    pub fn new(did: Did, max: u8) -> Self {
        Self {
            did,
            max,
            successors: Arc::new(RwLock::new(vec![])),
        }
    }

    /// Returns the list of successors in a read lock.
    pub fn successors(&self) -> Result<RwLockReadGuard<Vec<Did>>> {
        self.successors
            .read()
            .map_err(|_| Error::FailedToReadSuccessors)
    }

    /// Calculate bias of a node on the ring.
    pub fn bias(&self, did: Did) -> BiasId {
        BiasId::new(self.did, did)
    }

    /// Check if a node should be inserted into the sequence.
    pub fn should_insert(&self, did: Did) -> Result<bool> {
        if (self.contains(&did)?) || (did == self.did) {
            return Ok(false);
        }

        if self.bias(did) >= self.bias(self.max()?) && self.is_full()? {
            return Ok(false);
        }
        Ok(true)
    }
}

// Implementation of the SuccessorReader trait for SuccessorSeq
impl SuccessorReader for SuccessorSeq {
    /// Check if the specified Distributed Identifier (DID) exists in the successors list
    fn contains(&self, did: &Did) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.contains(did))
    }

    /// Check if the successors list is empty
    fn is_empty(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.is_empty())
    }

    /// Check if the successors list has reached its maximum capacity
    fn is_full(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.len() as u8 >= self.max)
    }

    /// Retrieve a successor from the list by index
    fn get(&self, index: usize) -> Result<Did> {
        let succs = self.successors()?;
        Ok(succs[index])
    }

    /// Return the length of the successors list
    fn len(&self) -> Result<usize> {
        let succs = self.successors()?;
        Ok(succs.len())
    }

    /// Retrieve the first successor in the list if not empty, otherwise return the node's DID
    fn min(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            Ok(self.get(0)?)
        }
    }

    /// Retrieve the last successor in the list if not empty, otherwise return the node's DID
    fn max(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            self.get(self.len()? - 1)
        }
    }

    /// Return a copy of the entire successors list
    fn list(&self) -> Result<Vec<Did>> {
        let succs = self.successors()?;
        Ok(succs.clone())
    }

    /// Simulate an update to the list and return the new DIDs that would be added
    fn update_dry(&self, dids: &[Did]) -> Result<Vec<Did>> {
        let mut ret = vec![];
        for did in dids {
            if self.should_insert(*did)? {
                ret.push(*did)
            }
        }
        Ok(ret)
    }
}

/// Implementation of `SuccessorWriter` for `SuccessorSeq`
impl SuccessorWriter for SuccessorSeq {
    /// Update the successors list by adding a new successor, sorting the list, and truncating if necessary
    fn update(&self, successor: Did) -> Result<Option<Did>> {
        if !(self.should_insert(successor)?) {
            return Ok(None);
        }
        let mut succs = self
            .successors
            .write()
            .map_err(|_| Error::FailedToWriteSuccessors)?;

        succs.push(successor);
        succs.sort(self.did);
        succs.truncate(self.max.into());
        if succs.contains(&successor) {
            Ok(Some(successor))
        } else {
            Ok(None)
        }
    }

    /// Extend the successors list with a list of new successors
    fn extend(&self, succ_list: &[Did]) -> Result<Vec<Did>> {
        let mut ret = vec![];
        for s in succ_list {
            if let Some(r) = self.update(*s)? {
                ret.push(r);
            }
        }
        Ok(ret)
    }

    /// Remove a successor from the successors list
    fn remove(&self, did: Did) -> Result<()> {
        let mut succs = self
            .successors
            .write()
            .map_err(|_| Error::FailedToWriteSuccessors)?;
        succs.retain(|&v| v != did);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::tests::gen_ordered_dids;

    #[test]
    fn test_successor_update() {
        let dids = gen_ordered_dids(6);

        let succ = SuccessorSeq::new(dids[0], 3);
        assert!(succ.is_empty().unwrap());

        succ.update(dids[2]).unwrap();
        assert_eq!(succ.list().unwrap(), dids[2..3]);

        succ.update(dids[3]).unwrap();
        assert_eq!(succ.list().unwrap(), dids[2..4]);

        succ.update(dids[4]).unwrap();
        assert_eq!(succ.list().unwrap(), dids[2..5]);

        succ.update(dids[5]).unwrap();
        assert_eq!(succ.list().unwrap(), dids[2..5]);

        succ.update(dids[1]).unwrap();
        assert_eq!(succ.list().unwrap(), dids[1..4]);
    }

    #[test]
    fn test_successor_remove() -> Result<()> {
        let dids = gen_ordered_dids(4);

        let succ = SuccessorSeq::new(dids[0], 3);
        assert!(succ.is_empty()?);

        succ.update(dids[1])?.unwrap();
        succ.update(dids[2])?.unwrap();
        succ.update(dids[3])?.unwrap();
        assert_eq!(succ.list()?, dids[1..4]);

        succ.remove(dids[2])?;
        assert_eq!(succ.list()?, vec![dids[1], dids[3]]);
        Ok(())
    }
}
