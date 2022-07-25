//! Successor for Chord
use crate::dht::did::SortRing;
use crate::dht::Did;

#[derive(Debug, Clone)]
pub struct Successor {
    /// self id
    id: Did,
    /// max successor
    max: u8,
    /// successor's list
    successors: Vec<Did>,
}

impl Successor {
    /// create a new Successor instance
    pub fn new(id: Did, max: u8) -> Self {
        Self {
            id,
            max,
            successors: vec![],
        }
    }

    pub fn is_none(&self) -> bool {
        self.successors.is_empty()
    }

    pub fn min(&self) -> Did {
        if self.is_none() {
            self.id
        } else {
            self.successors[0]
        }
    }

    pub fn max(&self) -> Did {
        if self.is_none() {
            self.id
        } else {
            self.successors[self.successors.len() - 1]
        }
    }

    pub fn update(&mut self, successor: Did) {
        if self.successors.contains(&successor) || successor == self.id {
            return;
        }
        self.successors.push(successor);
        self.successors.sort(self.id);
        self.successors.truncate(self.max.into());
    }

    pub fn list(&self) -> Vec<Did> {
        self.successors.clone()
    }

    pub fn remove(&mut self, id: Did) {
        self.successors.retain(|&v| v != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecc::tests::gen_ordered_keys;

    #[test]
    fn test_successor_update() {
        let dids: Vec<Did> = gen_ordered_keys(6)
            .iter()
            .map(|x| x.address().into())
            .collect();

        let mut succ = Successor::new(dids[0], 3);
        assert!(succ.is_none());

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
        let dids: Vec<Did> = gen_ordered_keys(4)
            .iter()
            .map(|x| x.address().into())
            .collect();

        let mut succ = Successor::new(dids[0], 3);
        assert!(succ.is_none());

        succ.update(dids[1]);
        succ.update(dids[2]);
        succ.update(dids[3]);
        assert_eq!(succ.list(), dids[1..4]);

        succ.remove(dids[2]);
        assert_eq!(succ.list(), vec![dids[1], dids[3]]);
    }
}
