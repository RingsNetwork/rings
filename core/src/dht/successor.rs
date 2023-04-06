//! Successor for PeerRing
use crate::dht::did::SortRing;
use crate::dht::Did;

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
    successors: Vec<Did>,
}

impl SuccessorSeq {
    pub fn new(did: Did, max: u8) -> Self {
        Self {
            did,
            max,
            successors: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.successors.is_empty()
    }

    pub fn is_full(&self) -> bool {
        self.successors.len() as u8 >= self.max
    }

    pub fn min(&self) -> Did {
        if self.is_empty() {
            self.did
        } else {
            self.successors[0]
        }
    }

    pub fn max(&self) -> Did {
        if self.is_empty() {
            self.did
        } else {
            self.successors[self.successors.len() - 1]
        }
    }

    pub fn update(&mut self, successor: Did) {
        if self.successors.contains(&successor) || successor == self.did {
            return;
        }
        self.successors.push(successor);
        self.successors.sort(self.did);
        self.successors.truncate(self.max.into());
    }

    pub fn extend(&mut self, succ_list: &Vec<Did>) {
        for s in succ_list {
            self.update(*s)
        }
    }

    pub fn list(&self) -> Vec<Did> {
        self.successors.clone()
    }

    pub fn remove(&mut self, did: Did) {
        self.successors.retain(|&v| v != did);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::tests::gen_ordered_dids;

    #[test]
    fn test_successor_update() {
        let dids = gen_ordered_dids(6);

        let mut succ = SuccessorSeq::new(dids[0], 3);
        assert!(succ.is_empty());

        succ.update(dids[2]);
        assert_eq!(succ.list(), dids[2..3]);

        succ.update(dids[3]);
        assert_eq!(succ.list(), dids[2..4]);

        succ.update(dids[4]);
        assert_eq!(succ.list(), dids[2..5]);

        succ.update(dids[5]);
        assert_eq!(succ.list(), dids[2..5]);

        succ.update(dids[1]);
        assert_eq!(succ.list(), dids[1..4]);
    }

    #[test]
    fn test_successor_remove() {
        let dids = gen_ordered_dids(4);

        let mut succ = SuccessorSeq::new(dids[0], 3);
        assert!(succ.is_empty());

        succ.update(dids[1]);
        succ.update(dids[2]);
        succ.update(dids[3]);
        assert_eq!(succ.list(), dids[1..4]);

        succ.remove(dids[2]);
        assert_eq!(succ.list(), vec![dids[1], dids[3]]);
    }
}
