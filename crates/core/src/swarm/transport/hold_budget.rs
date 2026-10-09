//! The node-wide bound of the session-link holds: frames waiting for a session their peer has
//! not backed yet have given their transport credit back, so no credit window bounds them, and
//! this budget bounds the holds of every connection together.

use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Frames all of a node's session-link holds keep at most (16 MiB of frames of at most
/// `MAX_DATA_CHANNEL_MESSAGE_SIZE` bytes). Each link's hold is bounded by its own capacity too;
/// this budget is what bounds their sum whatever the number of connections.
pub(crate) const NODE_SESSION_HOLD_CAPACITY: usize = 256;

/// The frames held across a node's session-link holds, bounded by
/// [`NODE_SESSION_HOLD_CAPACITY`]. Clone law: clones count the same frames.
#[derive(Clone)]
pub(crate) struct SessionHoldBudget(Arc<AtomicUsize>);

impl SessionHoldBudget {
    /// The budget of a node that holds nothing.
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Room for one more held frame, if the node has it; the permit returns it on drop.
    pub(crate) fn try_reserve(&self) -> Option<SessionHoldPermit> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < NODE_SESSION_HOLD_CAPACITY).then(|| held + 1)
            })
            .ok()
            .map(|_| SessionHoldPermit(Arc::clone(&self.0)))
    }
}

/// One frame counted in its node's [`SessionHoldBudget`] for as long as it is held.
pub(crate) struct SessionHoldPermit(Arc<AtomicUsize>);

impl Drop for SessionHoldPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod test_hold_budget {
    use super::SessionHoldBudget;
    use super::NODE_SESSION_HOLD_CAPACITY;

    /// Law (bound): the budget admits exactly `NODE_SESSION_HOLD_CAPACITY` held frames across
    /// its clones, refuses the next, and admits again once one leaves the hold.
    #[test]
    fn test_session_hold_budget_bounds_every_link_together() {
        let budget = SessionHoldBudget::new();
        let other_link = budget.clone();
        let mut held = (0..NODE_SESSION_HOLD_CAPACITY)
            .map(|index| {
                let link = if index % 2 == 0 { &budget } else { &other_link };
                link.try_reserve().expect("within capacity")
            })
            .collect::<Vec<_>>();
        assert!(budget.try_reserve().is_none());
        assert!(other_link.try_reserve().is_none());
        drop(held.pop());
        let _readmitted = other_link.try_reserve().expect("one frame left the hold");
        assert!(budget.try_reserve().is_none());
    }
}
