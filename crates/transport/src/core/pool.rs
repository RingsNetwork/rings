//! The pool of data channels of one connection, addressed by lane.
//!
//! A connection opens a fixed pool of ordered data channels. Every send names a [`ChannelLane`],
//! and the pool pins each lane to one channel:
//!
//! ```text
//! channel(lane) ≜ pool[lane mod |pool|]
//! ```
//!
//! **Law (lane order).** Messages sent on one lane travel on one ordered data channel, so they
//! reach the remote handler in send order, whatever happens on the other channels. A stalled
//! handler on one channel delays only its own lanes. Callers that need ordering (for example
//! one lane per traffic class, so that a class's sequence numbers arrive in order) get it from
//! this law; callers that do not can still spread load over lanes.

use std::sync::RwLock;

use crate::error::Error;
use crate::error::Result;

/// An ordering domain of one connection: all messages of a lane travel on one ordered channel.
///
/// The transport gives lanes no meaning beyond that; the caller chooses them.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChannelLane(u8);

impl ChannelLane {
    /// Name lane `index`.
    pub const fn new(index: u8) -> Self {
        Self(index)
    }

    /// The lane's index.
    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Selects the pooled resource a lane is pinned to.
pub trait LanePool<T> {
    /// The resource pinned to `lane`: the one at `lane mod |pool|`.
    fn select(&self, lane: ChannelLane) -> Result<T>;

    /// Check if all contained element of pool match the statement.
    fn all(&self, statement: fn(&T) -> bool) -> Result<bool>;
}

/// A pool of cloneable resources (the data channels of one connection), addressed by lane.
pub struct ChannelPool<T: Clone> {
    /// The pooled resources, in creation order.
    pool: RwLock<Vec<T>>,
}

impl<T: Clone> Default for ChannelPool<T> {
    fn default() -> Self {
        Self {
            pool: RwLock::new(vec![]),
        }
    }
}

impl<T: Clone> ChannelPool<T> {
    /// A pool of `resources`, in the order lanes address them.
    pub fn from_vec(resources: Vec<T>) -> Self {
        Self {
            pool: RwLock::new(resources),
        }
    }

    /// Append one resource; lanes address resources in the order they were pushed.
    pub fn push(&self, item: T) -> Result<()> {
        let mut pool = self
            .pool
            .write()
            .map_err(|_| Error::RwLockWrite("Failed to write channel pool".to_string()))?;
        pool.push(item);
        Ok(())
    }
}

impl<T: Clone> LanePool<T> for ChannelPool<T> {
    fn select(&self, lane: ChannelLane) -> Result<T> {
        let pool = self
            .pool
            .read()
            .map_err(|_| Error::RwLockRead("Failed to read channel pool".to_string()))?;
        let index = usize::from(lane.index())
            .checked_rem(pool.len())
            .ok_or(Error::ChannelPoolEmpty)?;
        pool.get(index).cloned().ok_or(Error::ChannelPoolEmpty)
    }

    fn all(&self, statement: fn(&T) -> bool) -> Result<bool> {
        let pool = self.pool.read().map_err(|_| {
            Error::RwLockRead("Failed to read channel pool when inspecting it".to_string())
        })?;
        Ok(pool.iter().all(statement))
    }
}

#[cfg(test)]
pub mod tests {
    //! Tests
    use super::ChannelLane;
    use super::ChannelPool;
    use super::LanePool;
    use crate::error::Error;

    /// Each lane always selects the same resource, `lane mod |pool|`.
    #[test]
    fn test_a_lane_is_pinned_to_one_resource() -> crate::error::Result<()> {
        let pool = ChannelPool::<usize>::from_vec(vec![10, 11, 12, 13]);
        for _ in 0..3 {
            for lane in 0..8_u8 {
                assert_eq!(
                    pool.select(ChannelLane::new(lane))?,
                    10 + usize::from(lane % 4)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn test_empty_pool_returns_typed_error() {
        let pool = ChannelPool::<usize>::default();
        assert!(matches!(
            pool.select(ChannelLane::default()),
            Err(Error::ChannelPoolEmpty)
        ));
    }
}
