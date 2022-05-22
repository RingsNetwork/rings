//! Successor for Chord
use crate::dht::did::SortRing;
use crate::dht::Did;

/// We can increase MAX_SUCCESSOR for more replication
const MAX_SUCCESSOR: u8 = 3;

#[derive(Debug, Clone)]
pub struct Successor {
    id: Did,
    max: u8,
    successors: Vec<Did>,
}

impl Successor {
    pub fn new(id: &Did) -> Self {
        Self {
            id: *id,
            max: MAX_SUCCESSOR,
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
        self.successors.retain(|v| *v == id);
    }
}
