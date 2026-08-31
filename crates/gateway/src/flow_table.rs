//! Bounded ownership table for non-terminal TCP flows.

use std::collections::HashMap;

use crate::FlowEvent;
use crate::FlowId;
use crate::FlowState;
use crate::FlowTableError;

/// Bounded table that owns every live flow state exactly once.
///
/// Invariant: `len() <= capacity()` and terminal states are never retained.
pub struct FlowTable {
    capacity: usize,
    states: HashMap<FlowId, FlowState>,
}

impl FlowTable {
    /// Create an empty table with a nonzero bound.
    pub fn new(capacity: usize) -> Result<Self, FlowTableError> {
        if capacity == 0 {
            return Err(FlowTableError::CapacityExhausted { limit: capacity });
        }
        if capacity > crate::config::MAX_GATEWAY_FLOWS {
            return Err(FlowTableError::CapacityLimitExceeded {
                requested: capacity,
                limit: crate::config::MAX_GATEWAY_FLOWS,
            });
        }
        let mut states = HashMap::new();
        states
            .try_reserve(capacity)
            .map_err(|_| FlowTableError::AllocationFailed { capacity })?;
        Ok(Self { capacity, states })
    }

    /// Return the configured concurrent flow bound.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Return the number of currently tracked non-terminal flows.
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Return whether no flow is currently tracked.
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Capture one previously unseen flow.
    pub fn capture(&mut self, id: FlowId) -> Result<FlowState, FlowTableError> {
        if self.states.contains_key(&id) {
            return Err(FlowTableError::Duplicate(id));
        }
        if self.states.len() >= self.capacity {
            return Err(FlowTableError::CapacityExhausted {
                limit: self.capacity,
            });
        }
        let state = FlowState::Captured(id);
        self.states.insert(id, state);
        Ok(state)
    }

    /// Return the current state for a tracked flow.
    pub fn state(&self, id: FlowId) -> Option<FlowState> {
        self.states.get(&id).copied()
    }

    /// Apply an event and remove the flow if it reaches a terminal state.
    pub fn transition(
        &mut self,
        id: FlowId,
        event: FlowEvent,
    ) -> Result<FlowState, FlowTableError> {
        let current = self
            .states
            .get(&id)
            .copied()
            .ok_or(FlowTableError::NotFound(id))?;
        let next = current.transition(event)?;
        if next.is_terminal() {
            self.states.remove(&id);
        } else {
            self.states.insert(id, next);
        }
        Ok(next)
    }

    /// Fail and release all currently tracked flows.
    pub fn fail_all(&mut self) -> Vec<FlowState> {
        self.states
            .drain()
            .map(|(id, _)| FlowState::Failed(id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(port: u16) -> FlowId {
        FlowId {
            source: format!("100.64.0.2:{port}").parse().expect("test source"),
            target: "93.184.216.34:443".parse().expect("test target"),
        }
    }

    #[test]
    fn table_never_exceeds_its_bound() {
        let mut table = FlowTable::new(1).expect("nonzero capacity");
        table.capture(flow(41_000)).expect("first flow fits");
        assert_eq!(
            table.capture(flow(41_001)),
            Err(FlowTableError::CapacityExhausted { limit: 1 })
        );
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn table_rejects_pathological_capacity_before_allocation() {
        assert!(matches!(
            FlowTable::new(usize::MAX),
            Err(FlowTableError::CapacityLimitExceeded { .. })
        ));
    }

    #[test]
    fn terminal_transition_releases_capacity() {
        let first = flow(41_000);
        let second = flow(41_001);
        let mut table = FlowTable::new(1).expect("nonzero capacity");
        table.capture(first).expect("first flow fits");
        table
            .transition(first, FlowEvent::Fail)
            .expect("captured flow may fail");
        assert!(table.is_empty());
        assert_eq!(table.capture(second), Ok(FlowState::Captured(second)));
    }

    #[test]
    fn fail_all_is_a_total_cleanup_transition() {
        let mut table = FlowTable::new(2).expect("nonzero capacity");
        table.capture(flow(41_000)).expect("first flow fits");
        table.capture(flow(41_001)).expect("second flow fits");
        let failed = table.fail_all();
        assert_eq!(failed.len(), 2);
        assert!(failed.into_iter().all(FlowState::is_terminal));
        assert!(table.is_empty());
    }
}
