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
//! **Law (edge watch, #855).** [`SlotRoster::count`] is the number of current edges, arrived and
//! not departed, whether or not they hold a slot. Every arrival or departure that changes it
//! notifies the listeners of [`SlotRoster::changed`] once the table is updated and its lock
//! released. So a waiter that takes its listener before it reads the count misses no change:
//!
//! ```text
//!   listener ← changed();  if p(count()) then done else await listener; repeat
//! ```
//!
//! [`SwarmEvent::PeerAdmitted`]: crate::swarm::SwarmEvent::PeerAdmitted
//! [`SwarmEvent::PeerRetired`]: crate::swarm::SwarmEvent::PeerRetired

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use event_listener::Event;
use event_listener::EventListener;

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

/// The slot assignment: the free slots, the slot of every current edge that holds one, and the
/// current edges that hold none.
#[derive(Debug, Default)]
struct SlotTable {
    /// The free slots; the least is given first.
    free: BTreeSet<u16>,
    /// The slot of every current edge that holds one.
    held: BTreeMap<Did, u16>,
    /// The current edges that arrived with no slot free.
    unslotted: BTreeSet<Did>,
}

/// The connection slots of a node (module documentation).
#[derive(Debug)]
pub(crate) struct SlotRoster {
    /// The slot assignment.
    table: Mutex<SlotTable>,
    /// Notified after every change of the current edges (the edge-watch law).
    changed: Event,
}

impl SlotRoster {
    /// A roster of `slots` free slots, `0..slots` (at most `u16::MAX + 1`).
    pub(crate) fn new(slots: usize) -> Self {
        let last = u16::try_from(slots.saturating_sub(1)).unwrap_or(u16::MAX);
        let free = match slots {
            0 => BTreeSet::new(),
            _ => (0..=last).collect(),
        };
        Self {
            table: Mutex::new(SlotTable {
                free,
                held: BTreeMap::new(),
                unslotted: BTreeSet::new(),
            }),
            changed: Event::new(),
        }
    }

    /// The slot table, for one pure step.
    fn table(&self) -> MutexGuard<'_, SlotTable> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Give `edge` the least free slot, unless it holds one. With no slot free, which the
    /// registry's bound of `R` connection records rules out, `edge` holds none and its
    /// deliveries are [`Principal::Unbound`]; it is a current edge all the same, and takes a slot
    /// at a later arrival that finds one free. Only an edge that was not current changes the
    /// count and notifies.
    pub(crate) fn arrive(&self, edge: Did) {
        let arrived = {
            let mut table = self.table();
            if table.held.contains_key(&edge) {
                return;
            }
            let was_current = table.unslotted.remove(&edge);
            match table.free.pop_first() {
                Some(slot) => {
                    table.held.insert(edge, slot);
                }
                None => {
                    table.unslotted.insert(edge);
                }
            }
            !was_current
        };
        if arrived {
            self.changed.notify(usize::MAX);
        }
    }

    /// Free the slot of `edge`, if it holds one; `edge` is no longer a current edge.
    pub(crate) fn depart(&self, edge: Did) {
        {
            let mut table = self.table();
            if let Some(slot) = table.held.remove(&edge) {
                table.free.insert(slot);
            } else if !table.unslotted.remove(&edge) {
                return;
            }
        }
        self.changed.notify(usize::MAX);
    }

    /// The number of current edges (the edge-watch law).
    pub(crate) fn count(&self) -> usize {
        let table = self.table();
        table.held.len().saturating_add(table.unslotted.len())
    }

    /// A listener woken by the next change of the current edges after this call (the edge-watch
    /// law).
    pub(crate) fn changed(&self) -> EventListener {
        self.changed.listen()
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
    use futures::FutureExt;

    use super::Principal;
    use super::SlotRoster;
    use crate::dht::Did;

    /// The edge-watch law: the count is the current edges, slotted or not, and a change notifies
    /// a listener taken before it; a repeated arrival or the departure of an unknown edge, which
    /// change nothing, notify none.
    #[test]
    fn test_the_edge_count_and_its_change_follow_arrivals_and_departures() {
        let roster = SlotRoster::new(1);
        let (a, b) = (Did::from(1_u32), Did::from(2_u32));
        assert_eq!(roster.count(), 0);

        let mut changed = roster.changed();
        roster.arrive(a);
        assert!(
            (&mut changed).now_or_never().is_some(),
            "an arrival notifies"
        );
        assert_eq!(roster.count(), 1);

        let mut unchanged = roster.changed();
        roster.arrive(a);
        roster.depart(b);
        assert!(
            (&mut unchanged).now_or_never().is_none(),
            "no change, no notification"
        );
        roster.arrive(b);
        assert_eq!(roster.count(), 2, "an edge with no slot free is current");
        assert_eq!(roster.principal_of(Some(b)), Principal::Unbound);
        assert!(unchanged.now_or_never().is_some());

        let departed = roster.changed();
        roster.depart(b);
        roster.depart(a);
        assert!(departed.now_or_never().is_some(), "a departure notifies");
        assert_eq!(roster.count(), 0);
    }

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
