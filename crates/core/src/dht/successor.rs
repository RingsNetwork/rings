//! Successor list of the local node.
//!
//! The list is the committed projection of [`TopologyState::successors`](crate::dht::topology::TopologyState):
//! every value it holds was produced by [`topology::step`](crate::dht::topology::step),
//! which is the sole owner of its order, deduplication, and capacity law
//! (`successors(known, local, capacity)`). The container only serialises reads
//! against the transition commit; it never re-derives that law.
#![deny(missing_docs)]
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;

#[cfg(test)]
use crate::dht::topology;
use crate::dht::Did;
use crate::error::Error;
use crate::error::Result;

/// Committed successor list with a fixed capacity `r`.
///
/// Invariant: the held vector is exactly the last `successors` value the
/// topology transition committed; it is sorted by clockwise distance from the
/// local node, contains neither duplicates nor the local node, and holds at
/// most `capacity()` entries.
#[derive(Debug, Clone)]
pub struct SuccessorSeq {
    /// Local identity defining clockwise order and the empty-list fallback.
    did: Did,
    /// Fixed upper bound on the number of committed successors.
    max: u8,
    /// Shared committed list; cloned views observe the same topology updates.
    successors: Arc<RwLock<Vec<Did>>>,
}

impl SuccessorSeq {
    /// An empty list for `did` with capacity `max`.
    pub fn new(did: Did, max: u8) -> Self {
        Self {
            did,
            max,
            successors: Arc::new(RwLock::new(vec![])),
        }
    }

    /// Borrow the committed list, reporting poisoning at the read boundary.
    fn successors(&self) -> Result<RwLockReadGuard<'_, Vec<Did>>> {
        self.successors
            .read()
            .map_err(|_| Error::FailedToReadSuccessors)
    }

    /// The capacity `r` of the list.
    pub fn capacity(&self) -> usize {
        self.max.into()
    }

    /// Commit the successor list of one topology transition.
    ///
    /// Pre: `successors` is the normalised list produced by
    /// [`topology::step`](crate::dht::topology::step).
    pub(crate) fn replace_state(&self, successors: &[Did]) -> Result<()> {
        *self
            .successors
            .write()
            .map_err(|_| Error::FailedToWriteSuccessors)? = successors.to_vec();
        Ok(())
    }

    /// Whether `did` is a committed successor.
    pub fn contains(&self, did: &Did) -> Result<bool> {
        Ok(self.successors()?.contains(did))
    }

    /// Whether the node currently knows no successor.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.successors()?.is_empty())
    }

    /// The successor at `index` in clockwise order.
    pub fn get(&self, index: usize) -> Result<Did> {
        let succs = self.successors()?;
        succs
            .get(index)
            .copied()
            .ok_or(Error::SuccessorIndexOutOfBounds {
                index,
                len: succs.len(),
            })
    }

    /// The successor head, or the local node when the list is empty.
    pub fn min(&self) -> Result<Did> {
        Ok(self.successors()?.first().copied().unwrap_or(self.did))
    }

    /// A copy of the committed list.
    pub fn list(&self) -> Result<Vec<Did>> {
        Ok(self.successors()?.clone())
    }
}

/// Direct test setup that bypasses the topology transition.
///
/// Every write is normalised by the same [`topology::successors`] law the
/// transition uses, so a test fixture can never hold a list `step` would not
/// produce.
#[cfg(test)]
impl SuccessorSeq {
    /// Insert `successor`; `Some` when it was retained under the capacity.
    pub(crate) fn update(&self, successor: Did) -> Result<Option<Did>> {
        let retained = self.extend(&[successor])?;
        Ok(retained.into_iter().next())
    }

    /// Insert every candidate; returns the newly retained ones in list order.
    pub(crate) fn extend(&self, candidates: &[Did]) -> Result<Vec<Did>> {
        let before = self.list()?;
        let mut known = before.clone();
        known.extend_from_slice(candidates);
        let next = topology::successors(&known, self.did, self.capacity());
        self.replace_state(&next)?;
        Ok(next
            .into_iter()
            .filter(|did| !before.contains(did))
            .collect())
    }

    /// Drop `did` from the list.
    pub(crate) fn remove(&self, did: Did) -> Result<()> {
        let next = self
            .list()?
            .into_iter()
            .filter(|current| *current != did)
            .collect::<Vec<_>>();
        self.replace_state(&next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::tests::gen_ordered_dids;

    #[test]
    fn test_replace_state_commits_the_transition_value_verbatim() -> Result<()> {
        let dids = gen_ordered_dids(4);
        let succ = SuccessorSeq::new(dids[0], 3);
        assert!(succ.is_empty()?);
        assert_eq!(succ.min()?, dids[0]);

        succ.replace_state(&dids[1..4])?;

        assert_eq!(succ.list()?, dids[1..4]);
        assert_eq!(succ.min()?, dids[1]);
        assert_eq!(succ.get(2)?, dids[3]);
        assert!(succ.contains(&dids[2])?);
        assert_eq!(succ.capacity(), 3);
        Ok(())
    }

    #[test]
    fn test_writer_follows_the_transition_law() -> Result<()> {
        let dids = gen_ordered_dids(7);
        let succ = SuccessorSeq::new(dids[4], 3);

        succ.extend(&[dids[2], dids[6], dids[0], dids[5], dids[3], dids[1]])?;
        assert_eq!(succ.list()?, vec![dids[5], dids[6], dids[0]]);

        assert_eq!(succ.update(dids[1])?, None);
        succ.remove(dids[6])?;
        assert_eq!(succ.list()?, vec![dids[5], dids[0]]);
        assert_eq!(succ.update(dids[6])?, Some(dids[6]));
        assert_eq!(succ.list()?, vec![dids[5], dids[6], dids[0]]);
        Ok(())
    }

    #[test]
    fn test_get_out_of_bounds_returns_typed_error() {
        let dids = gen_ordered_dids(1);
        let succ = SuccessorSeq::new(dids[0], 3);

        assert!(matches!(
            succ.get(0),
            Err(Error::SuccessorIndexOutOfBounds { index: 0, len: 0 })
        ));
    }
}
