//! The connection slots of a node, which every table under slot accounting charges (load-bounds
//! paper, Def. 1 and Law 1).
//!
//! A node has `R` connection slots, `0..R`. The transport announces an edge's admission and
//! retirement in the one critical section that decides it ([`SwarmEvent::PeerAdmitted`] and
//! [`SwarmEvent::PeerRetired`]); there it calls [`SlotRoster::arrive`], which gives the edge the
//! least free slot, and [`SlotRoster::depart`], which frees it.
//!
//! **Law (slots).** The current edges hold distinct slots, and a slot is held by at most one
//! edge at a time. A slot is a unit of capacity, not a topology reference: one edge may back
//! several fingers and a successor, or none. A table's items stay charged to their slot after its
//! edge departs, and an edge later given that slot inherits them (Prop. 1, rotation residual).
//!
//! **Lock order.** The roster's lock is a leaf: taken under the transport's lifecycle boundary,
//! or alone to name a delivery's slot, for one pure step.
//!
//! [`SwarmEvent::PeerAdmitted`]: crate::swarm::SwarmEvent::PeerAdmitted
//! [`SwarmEvent::PeerRetired`]: crate::swarm::SwarmEvent::PeerRetired

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use super::Principal;
use super::Shares;
use super::SlotAccounting;
use crate::dht::Did;

/// The accounting of one table, shared by the table's effect boundary and its undo log.
#[derive(Clone, Debug)]
pub(crate) struct TableAccounting(Arc<Mutex<SlotAccounting>>);

impl TableAccounting {
    /// An empty table with `shares`.
    pub(crate) fn new(shares: Shares) -> Self {
        Self(Arc::new(Mutex::new(SlotAccounting::new(shares))))
    }

    /// The accounting, for one pure step.
    ///
    /// Lock law: held for one pure step and never across a suspension point; a poisoned lock
    /// still guards a well-formed state, since no step panics between two writes.
    pub(crate) fn lock(&self) -> MutexGuard<'_, SlotAccounting> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The slot assignment: the free slots and the slot of every current edge.
#[derive(Debug, Default)]
struct SlotTable {
    /// The free slots; the least is given first.
    free: BTreeSet<u16>,
    /// The slot of every current edge.
    held: BTreeMap<Did, u16>,
}

/// The connection slots of a node (module documentation).
#[derive(Debug)]
pub(crate) struct SlotRoster(Mutex<SlotTable>);

impl SlotRoster {
    /// A roster of `slots` free slots, `0..slots` (at most `u16::MAX + 1`).
    pub(crate) fn new(slots: usize) -> Self {
        let last = u16::try_from(slots.saturating_sub(1)).unwrap_or(u16::MAX);
        let free = match slots {
            0 => BTreeSet::new(),
            _ => (0..=last).collect(),
        };
        Self(Mutex::new(SlotTable {
            free,
            held: BTreeMap::new(),
        }))
    }

    /// The slot table, for one pure step.
    fn table(&self) -> MutexGuard<'_, SlotTable> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Give `edge` the least free slot, unless it holds one. With no slot free, which the
    /// registry's bound of `R` connection records rules out, `edge` holds none and its
    /// deliveries are [`Principal::Unbound`].
    pub(crate) fn arrive(&self, edge: Did) {
        let mut table = self.table();
        if table.held.contains_key(&edge) {
            return;
        }
        if let Some(slot) = table.free.pop_first() {
            table.held.insert(edge, slot);
        }
    }

    /// Free the slot of `edge`, if it holds one.
    pub(crate) fn depart(&self, edge: Did) {
        let mut table = self.table();
        if let Some(slot) = table.held.remove(&edge) {
            table.free.insert(slot);
        }
    }

    /// The principal a delivery on `edge` is charged to: its slot, or [`Principal::Unbound`]
    /// for no edge or an edge that holds none.
    pub(crate) fn principal_of(&self, edge: Option<Did>) -> Principal {
        edge.and_then(|edge| self.table().held.get(&edge).copied())
            .map_or(Principal::Unbound, Principal::Slot)
    }
}

#[cfg(test)]
mod tests {
    use super::Principal;
    use super::SlotRoster;
    use crate::dht::Did;

    /// The least free slot is given first, a departure frees its slot for the next arrival,
    /// a repeated arrival keeps its slot, and an edge with no slot free is unbound.
    #[test]
    fn test_slots_are_given_least_first_and_reused() {
        let roster = SlotRoster::new(2);
        let (a, b, c) = (Did::from(1_u32), Did::from(2_u32), Did::from(3_u32));
        roster.arrive(a);
        roster.arrive(b);
        roster.arrive(a);
        assert_eq!(roster.principal_of(Some(a)), Principal::Slot(0));
        assert_eq!(roster.principal_of(Some(b)), Principal::Slot(1));
        roster.arrive(c);
        assert_eq!(roster.principal_of(Some(c)), Principal::Unbound);
        roster.depart(a);
        assert_eq!(roster.principal_of(Some(a)), Principal::Unbound);
        roster.arrive(c);
        assert_eq!(roster.principal_of(Some(c)), Principal::Slot(0));
        assert_eq!(roster.principal_of(None), Principal::Unbound);
    }
}
