//! Successor for PeerRing
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
    /// Node did
    did: Did,
    /// Max successor num
    max: u8,
    /// Successors
    successors: Arc<RwLock<Vec<Did>>>,
}

pub trait SuccessorReader {
    fn is_empty(&self) -> Result<bool>;
    fn is_full(&self) -> Result<bool>;
    fn get(&self, index: usize) -> Result<Did>;
    fn len(&self) -> Result<usize>;
    fn min(&self) -> Result<Did>;
    fn max(&self) -> Result<Did>;
    fn list(&self) -> Result<Vec<Did>>;
    fn contains(&self, did: &Did) -> Result<bool>;
}

pub trait SuccessorWriter {
    fn update(&self, successor: Did) -> Result<Option<Did>>;
    fn extend(&self, succ_list: &Vec<Did>) -> Result<Vec<Did>>;
    fn remove(&self, did: Did) -> Result<()>;
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

    /// Calculate bias of the Did on the ring.
    pub fn bias(&self, did: Did) -> BiasId {
        BiasId::new(self.did, did)
    }
}

impl SuccessorReader for SuccessorSeq {
    fn contains(&self, did: &Did) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.contains(&did))
    }

    fn is_empty(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.is_empty())
    }

    fn is_full(&self) -> Result<bool> {
        let succs = self.successors()?;
        Ok(succs.len() as u8 >= self.max)
    }

    fn get(&self, index: usize) -> Result<Did> {
        let succs = self.successors()?;
        Ok(succs[index])
    }

    fn len(&self) -> Result<usize> {
        let succs = self.successors()?;
        Ok(succs.len())
    }

    fn min(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            Ok(self.get(0)?)
        }
    }

    fn max(&self) -> Result<Did> {
        if self.is_empty()? {
            Ok(self.did)
        } else {
            self.get(self.len()? - 1)
        }
    }

    fn list(&self) -> Result<Vec<Did>> {
        let succs = self.successors()?;
        Ok(succs.clone())
    }
}

impl SuccessorWriter for SuccessorSeq {
    fn update(&self, successor: Did) -> Result<Option<Did>> {
        // if successor in successor list
        // or successor is self
        // or list is full
        // or successor is bigger than successor.max()
        if (self.contains(&successor)?) || (successor == self.did) {
            return Ok(None);
        }

        if self.bias(successor) >= self.bias(self.max()?) && self.is_full()? {
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

    fn extend(&self, succ_list: &Vec<Did>) -> Result<Vec<Did>> {
        let mut ret = vec![];
        for s in succ_list {
            if let Some(r) = self.update(*s)? {
                ret.push(r);
            }
        }
        Ok(ret)
    }

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
