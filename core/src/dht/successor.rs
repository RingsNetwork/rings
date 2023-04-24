//! Successor for PeerRing
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;

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
    /// Node did
    did: Did,
    /// Max successor num
    max: u8,
    /// Successors
    successors: Arc<RwLock<Vec<Did>>>,
}

impl SuccessorSeq {
    pub fn new(did: Did, max: u8) -> Self {
        Self {
            did,
            max,
            successors: Arc::new(RwLock::new(vec![])),
        }
    }

    pub fn successors(&self) -> Result<RwLockReadGuard<Vec<Did>>> {
        self.successors
            .read()
            .map_err(|_| Error::FailedToReadSuccessors)
    }

    pub fn is_empty(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.is_empty())
    }

    pub fn is_full(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.len() as u8 >= self.max)
    }

    pub fn get(&self, index: usize) -> Result<Did> {
        let succs = self.successors()?;
        Ok(succs[index])
    }

    pub fn len(&self) -> Result<usize> {
        let succs = self.successors()?;
        Ok(succs.len())
    }

    pub fn min(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            Ok(self.get(0)?)
        }
    }

    pub fn max(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            self.get(self.len()? - 1)
        }
    }

    pub fn list(&self) -> Result<Vec<Did>> {
        let succs = self.successors()?;
        Ok(succs.clone())
    }

    pub fn update(&self, successor: Did) -> Result<()> {
        let mut succs = self
            .successors
            .write()
            .map_err(|_| Error::FailedToWriteSuccessors)?;
        if succs.contains(&successor) || successor == self.did {
            return Ok(());
        }
        succs.push(successor);
        succs.sort(self.did);
        succs.truncate(self.max.into());
        Ok(())
    }

    pub fn extend(&self, succ_list: &Vec<Did>) -> Result<()> {
        for s in succ_list {
            self.update(*s)?;
        }
        Ok(())
    }

    pub fn remove(&self, did: Did) -> Result<()> {
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

        succ.update(dids[1])?;
        succ.update(dids[2])?;
        succ.update(dids[3])?;
        assert_eq!(succ.list()?, dids[1..4]);

        succ.remove(dids[2]);
        assert_eq!(succ.list()?, vec![dids[1], dids[3]]);
        Ok(())
    }
}
