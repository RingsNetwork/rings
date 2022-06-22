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
    /// create a new Successer instance
    pub fn new(id: &Did, max: u8) -> Self {
        Self {
            id: *id,
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
        self.successors.retain(|v| *v == id);
    }
}
