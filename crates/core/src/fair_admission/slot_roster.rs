//! The connection slots of a node, which every table under slot accounting charges (load-bounds
//! paper, Def. 1 and Law 1).
//!
//! A node has `R` connection slots, `0..R`. The transport announces an edge's admission and
//! retirement in the one critical section that decides it ([`SwarmEvent::PeerAdmitted`] and
//! [`SwarmEvent::PeerRetired`]); there it calls [`SlotRoster::arrive`], which gives the edge the
//! cleanest free slot, and [`SlotRoster::depart`], which frees it.
//!
//! **Law (slots).** The current edges hold distinct slots, and a slot is held by at most one
//! edge at a time. A slot is a unit of capacity, not a topology reference: one edge may back
//! several fingers and a successor, or none. A table's items stay charged to their slot after its
//! edge departs, and an edge later given that slot inherits them (Prop. 1, rotation residual).
//!
//! **Law (cleanest first, #1094).** An arriving edge takes the free slot of least floor use
//! (paper, Def. 1), the least index among equals. It inherits a charged floor only when every
//! free slot is charged, so a party that rotates edges must charge all of them to reach a
//! newcomer; under least-free, one departure would reach the next arrival (Prop. 1(3)).
//!
//! ```text
//! arrive(e, w):  κ = argmin_{κ ∈ free} (w(κ), κ);   free −= {κ};   held[e] = κ
//! ```
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
use super::SlotFloorUse;
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

    /// The floor use of every slot at `now_ms` ([`SlotAccounting::floor_use_at`]), in one pure
    /// step.
    pub(crate) fn floor_use_at(&self, now_ms: u128) -> SlotFloorUse {
        self.lock().floor_use_at(now_ms)
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

    /// Give `edge` the cleanest free slot under `floor_use` (module law), unless it holds one.
    /// With no slot free, which the registry's bound of `R` connection records rules out, `edge`
    /// holds none and its deliveries are [`Principal::Unbound`]; it is a current edge all the
    /// same, and takes a slot at a later arrival that finds one free. Only an edge that was not
    /// current changes the count and notifies (the edge-watch law). `O(R log R)`.
    pub(crate) fn arrive(&self, edge: Did, floor_use: &SlotFloorUse) {
        let arrived = {
            let mut table = self.table();
            if table.held.contains_key(&edge) {
                return;
            }
            let was_current = table.unslotted.remove(&edge);
            let cleanest = table
                .free
                .iter()
                .copied()
                .min_by_key(|slot| (floor_use.of(*slot), *slot));
            match cleanest {
                Some(slot) => {
                    table.free.remove(&slot);
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
    use super::Shares;
    use super::SlotAccounting;
    use super::SlotFloorUse;
    use super::SlotRoster;
    use crate::dht::Did;
    use crate::fair_admission::slot_accounting::FloorUse;

    /// A table with a floor of 2 and room enough that no test meets its pool.
    const SHARES: Shares = Shares {
        floor: 2,
        pool: 1024,
        local: 0,
        inbox: 0,
        custody: 0,
    };

    /// The instant every test item leaves at, and an instant before it.
    const UNTIL_MS: u128 = 1_000;
    const NOW_MS: u128 = 10;

    /// Charge the slot of `edge` its whole floor in `table`.
    fn fill_floor(roster: &SlotRoster, table: &mut SlotAccounting, edge: Did) {
        let principal = roster.principal_of(Some(edge));
        (0..SHARES.floor).for_each(|_| table.charge(principal, UNTIL_MS));
    }

    /// The edge-watch law: the count is the current edges, slotted or not, and a change notifies
    /// a listener taken before it; a repeated arrival or the departure of an unknown edge, which
    /// change nothing, notify none.
    #[test]
    fn test_the_edge_count_and_its_change_follow_arrivals_and_departures() {
        let roster = SlotRoster::new(1);
        let clean = SlotFloorUse::default();
        let (a, b) = (Did::from(1_u32), Did::from(2_u32));
        assert_eq!(roster.count(), 0);

        let mut changed = roster.changed();
        roster.arrive(a, &clean);
        assert!(
            (&mut changed).now_or_never().is_some(),
            "an arrival notifies"
        );
        assert_eq!(roster.count(), 1);

        let mut unchanged = roster.changed();
        roster.arrive(a, &clean);
        roster.depart(b);
        assert!(
            (&mut unchanged).now_or_never().is_none(),
            "no change, no notification"
        );
        roster.arrive(b, &clean);
        assert_eq!(roster.count(), 2, "an edge with no slot free is current");
        assert_eq!(roster.principal_of(Some(b)), Principal::Unbound);
        assert!(unchanged.now_or_never().is_some());

        let departed = roster.changed();
        roster.depart(b);
        roster.depart(a);
        assert!(departed.now_or_never().is_some(), "a departure notifies");
        assert_eq!(roster.count(), 0);
    }

    /// Among clean slots the least is given first, a departure frees its slot for the next
    /// arrival, a repeated arrival keeps its slot, and an edge with no slot free is unbound.
    #[test]
    fn test_slots_are_given_least_first_among_clean_and_reused() {
        let roster = SlotRoster::new(2);
        let clean = SlotFloorUse::default();
        let (a, b, c) = (Did::from(1_u32), Did::from(2_u32), Did::from(3_u32));
        roster.arrive(a, &clean);
        roster.arrive(b, &clean);
        roster.arrive(a, &clean);
        assert_eq!(roster.principal_of(Some(a)), Principal::Slot(0));
        assert_eq!(roster.principal_of(Some(b)), Principal::Slot(1));
        roster.arrive(c, &clean);
        assert_eq!(roster.principal_of(Some(c)), Principal::Unbound);
        roster.depart(a);
        assert_eq!(roster.principal_of(Some(a)), Principal::Unbound);
        roster.arrive(c, &clean);
        assert_eq!(roster.principal_of(Some(c)), Principal::Slot(0));
        assert_eq!(roster.principal_of(None), Principal::Unbound);
    }

    /// An arrival takes the free slot of least floor use: a clean one before a half-charged
    /// one before a full one, and an item past its instant counts as gone.
    #[test]
    fn test_an_arrival_takes_the_cleanest_free_slot() {
        let roster = SlotRoster::new(3);
        let mut table = SlotAccounting::new(SHARES);
        table.charge(Principal::Slot(0), UNTIL_MS);
        table.charge(Principal::Slot(0), UNTIL_MS);
        table.charge(Principal::Slot(1), UNTIL_MS);
        let (a, b, c) = (Did::from(1_u32), Did::from(2_u32), Did::from(3_u32));

        roster.arrive(a, &table.floor_use_at(NOW_MS));
        roster.arrive(b, &table.floor_use_at(NOW_MS));
        roster.arrive(c, &table.floor_use_at(NOW_MS));
        assert_eq!(roster.principal_of(Some(a)), Principal::Slot(2));
        assert_eq!(roster.principal_of(Some(b)), Principal::Slot(1));
        assert_eq!(roster.principal_of(Some(c)), Principal::Slot(0));

        roster.depart(b);
        roster.depart(c);
        roster.arrive(b, &table.floor_use_at(UNTIL_MS));
        assert_eq!(roster.principal_of(Some(b)), Principal::Slot(0));
    }

    /// Rotation residual (paper, Prop. 1(1) and (3)), exhaustive over `R ≤ 8` slots and
    /// `n ≤ R` rotations: a party that rotates one identity at a time, filling its slot's
    /// floor before each departure, reaches an honest newcomer under cleanest-first only after
    /// charging every free slot (`n ≥ R`), and under least-free after one rotation. In both,
    /// the newcomer inherits the least floor use over the free slots.
    #[test]
    fn test_one_rotating_identity_reaches_a_newcomer_only_through_every_free_slot() {
        for slots in 1..=8_u16 {
            for rotations in 0..=slots {
                for cleanest in [true, false] {
                    let roster = SlotRoster::new(usize::from(slots));
                    let mut table = SlotAccounting::new(SHARES);
                    let snapshot = |table: &SlotAccounting| match cleanest {
                        true => table.floor_use_at(NOW_MS),
                        false => SlotFloorUse::default(),
                    };
                    for rotation in 0..u32::from(rotations) {
                        let attacker = Did::from(100 + rotation);
                        roster.arrive(attacker, &snapshot(&table));
                        fill_floor(&roster, &mut table, attacker);
                        roster.depart(attacker);
                    }
                    let least_free_use = (0..slots)
                        .map(|slot| table.floor_use_at(NOW_MS).of(slot))
                        .min();

                    let honest = Did::from(1_u32);
                    roster.arrive(honest, &snapshot(&table));
                    let Principal::Slot(slot) = roster.principal_of(Some(honest)) else {
                        panic!("a free slot is given");
                    };
                    let inherited = table.floor_use_at(NOW_MS).of(slot);
                    let charged = inherited > FloorUse::CLEAN;

                    let reached = match cleanest {
                        true => rotations >= slots,
                        false => rotations >= 1,
                    };
                    assert_eq!(charged, reached, "R = {slots}, n = {rotations}");
                    if cleanest {
                        assert_eq!(Some(inherited), least_free_use);
                    }
                }
            }
        }
    }

    /// An honest joining storm, `R` arrivals at once (a seed after a restart), is admitted
    /// whole: every arrival takes a distinct slot, whatever the tables hold.
    #[test]
    fn test_a_joining_storm_takes_every_slot_at_once() {
        let slots = 8_u16;
        let roster = SlotRoster::new(usize::from(slots));
        let mut table = SlotAccounting::new(SHARES);
        (0..slots).for_each(|slot| table.charge(Principal::Slot(slot), UNTIL_MS));
        let edges: Vec<Did> = (0..u32::from(slots)).map(Did::from).collect();
        edges
            .iter()
            .for_each(|edge| roster.arrive(*edge, &table.floor_use_at(NOW_MS)));
        let mut given: Vec<Principal> = edges
            .iter()
            .map(|edge| roster.principal_of(Some(*edge)))
            .collect();
        given.sort();
        let all: Vec<Principal> = (0..slots).map(Principal::Slot).collect();
        assert_eq!(given, all);
    }
}
