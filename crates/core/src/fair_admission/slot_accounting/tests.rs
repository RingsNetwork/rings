//! The laws of slot accounting, checked over every reachable state of a small table and on the
//! scenarios the load-bounds paper states.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;

use super::Principal;
use super::ShareRefusal;
use super::Shares;
use super::SlotAccounting;

/// The small table of the exhaustive check: `f = 1`, `P = 2`, `L = 1`.
const SMALL: Shares = Shares {
    floor: 1,
    pool: 2,
    local: 1,
    inbox: 0,
    custody: 0,
};
/// `R` of the small table.
const SMALL_SLOTS: u16 = 2;
/// The instants items of the small table leave at.
const INSTANTS: [u128; 2] = [1, 2];

/// Every principal of the small table.
fn principals() -> Vec<Principal> {
    (0..SMALL_SLOTS)
        .map(Principal::Slot)
        .chain([Principal::Local, Principal::Unbound])
        .collect()
}

/// One transition of the model.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// An item is admitted and charged, if the law admits it.
    Introduce(Principal, u128),
    /// An item leaves before its instant.
    Discharge(Principal, u128),
    /// The clock passes an instant.
    Advance(u128),
}

/// Every transition from any state of the small table.
fn steps() -> Vec<Step> {
    let mut steps = Vec::new();
    for principal in principals() {
        for instant in INSTANTS {
            steps.extend([
                Step::Introduce(principal, instant),
                Step::Discharge(principal, instant),
            ]);
        }
    }
    steps.extend(INSTANTS.map(Step::Advance));
    steps
}

/// The successor of `state` under `step` (an inadmissible step leaves it as it is).
fn after(state: &SlotAccounting, step: Step) -> SlotAccounting {
    let mut next = state.clone();
    match step {
        Step::Introduce(principal, instant) => {
            if next.admit(principal).is_ok() {
                next.charge(principal, instant);
            }
        }
        Step::Discharge(principal, instant) => next.discharge(principal, instant),
        Step::Advance(instant) => next.advance(instant),
    }
    next
}

/// The aggregates recomputed from the items alone: `(Σo, claimants)`.
fn recomputed(state: &SlotAccounting) -> (usize, usize) {
    principals()
        .into_iter()
        .map(|principal| state.pool_items(principal))
        .fold((0, 0), |(pool, claimants), items| {
            (pool + items, claimants + usize::from(items > 0))
        })
}

/// Check every law on one reachable state.
fn check(state: &SlotAccounting) {
    let held = state.expiry.values().sum::<usize>();
    assert_eq!(held, state.len(), "{state:?}");
    assert_eq!(
        (state.pool_held, state.claimants),
        recomputed(state),
        "{state:?}"
    );
    // The bound: |T| ≤ R·f + P + L.
    assert!(
        state.len() <= SMALL.bound(usize::from(SMALL_SLOTS)),
        "{state:?}"
    );
    for principal in principals() {
        let verdict = state.admit(principal);
        // Isolation: a slot below its floor is always admitted.
        if matches!(principal, Principal::Slot(_)) && state.held(principal) < SMALL.floor {
            assert_eq!(verdict, Ok(()), "{principal:?} {state:?}");
        }
        // The fair claim is never below `⌈P / (R + 1)⌉`, the unbound class included.
        if let Err(ShareRefusal::FairClaim { held, claim }) = verdict {
            let slots = usize::from(SMALL_SLOTS);
            assert!(claim >= SMALL.pool.div_ceil(slots + 1), "{state:?}");
            assert!(held >= claim, "{state:?}");
        }
    }
}

/// Every law holds on every state the small table can reach, from the empty one.
#[test]
fn test_every_reachable_state_keeps_the_accounting_laws() {
    let initial = SlotAccounting::new(SMALL);
    let canonical = |state: &SlotAccounting| state.expiry.clone();
    let mut seen = BTreeSet::<BTreeMap<(u128, Principal), usize>>::from([canonical(&initial)]);
    let mut frontier = VecDeque::from([initial]);
    let steps = steps();
    while let Some(state) = frontier.pop_front() {
        check(&state);
        for step in steps.iter() {
            let next = after(&state, *step);
            if seen.insert(canonical(&next)) {
                frontier.push_back(next);
            }
        }
    }
    // Floors, pool, the unbound class and the local share were all reached.
    assert!(seen.len() > 100, "{}", seen.len());
}

/// The replay table's shares (load-bounds paper, Law 2).
const REPLAY: Shares = Shares {
    floor: 32,
    pool: 4096,
    local: 1024,
    inbox: 0,
    custody: 0,
};

/// Fill `slot`'s floor with items leaving at `until_ms`.
fn fill_floor(accounting: &mut SlotAccounting, slot: u16, until_ms: u128) {
    for _ in 0..REPLAY.floor {
        assert_eq!(accounting.admit(Principal::Slot(slot)), Ok(()));
        accounting.charge(Principal::Slot(slot), until_ms);
    }
}

/// Rotation residual (Prop. 1): a slot's items outlive the edge that charged them, so an edge
/// later given that slot finds no floor, only its fair claim, until they leave; every other slot
/// keeps its floor, and the table never passes its bound however many slots are charged.
#[test]
fn test_a_slot_keeps_its_charges_until_they_leave() {
    let slots = 328_u16;
    let mut accounting = SlotAccounting::new(REPLAY);
    for slot in 0..slots - 1 {
        fill_floor(&mut accounting, slot, 1);
    }
    let bound = REPLAY.bound(usize::from(slots));
    assert!(accounting.len() <= bound);
    // The inherited slot has no floor, but a fair claim of the empty pool.
    assert_eq!(accounting.admit(Principal::Slot(0)), Ok(()));
    accounting.charge(Principal::Slot(0), 1);
    assert_eq!(accounting.pool_items(Principal::Slot(0)), 1);
    // The slot no one charged keeps its floor.
    fill_floor(&mut accounting, slots - 1, 1);
    // Once the items leave, the inherited slot has its floor again.
    accounting.advance(1);
    assert_eq!(accounting.len(), 0);
    fill_floor(&mut accounting, 0, 2);
}

/// The fair claim: one over-floor slot alone may take the whole pool, and a second claimant
/// asking splits what frees, each claimant gaining nothing past `⌈P / n⌉`.
#[test]
fn test_a_lone_claimant_takes_the_pool_and_a_second_splits_what_frees() {
    let shares = Shares {
        floor: 1,
        pool: 4,
        local: 0,
        inbox: 0,
        custody: 0,
    };
    let mut accounting = SlotAccounting::new(shares);
    let (relay, other) = (Principal::Slot(0), Principal::Slot(1));
    for _ in 0..5 {
        assert_eq!(accounting.admit(relay), Ok(()));
        accounting.charge(relay, 9);
    }
    assert_eq!(
        accounting.admit(relay),
        Err(ShareRefusal::PoolFull { held: 4, pool: 4 })
    );
    // The other slot's floor is apart from the pool.
    assert_eq!(accounting.admit(other), Ok(()));
    accounting.charge(other, 9);
    // Two items of the relay leave; the other slot, asking, is a second claimant: ⌈4/2⌉ = 2.
    accounting.discharge(relay, 9);
    accounting.discharge(relay, 9);
    for _ in 0..2 {
        assert_eq!(accounting.admit(other), Ok(()));
        accounting.charge(other, 9);
    }
    assert_eq!(
        accounting.admit(relay),
        Err(ShareRefusal::PoolFull { held: 4, pool: 4 })
    );
    // One more relay item leaves: the relay, below its claim, gains it back; the other, at its
    // claim, does not.
    accounting.discharge(relay, 9);
    assert_eq!(
        accounting.admit(other),
        Err(ShareRefusal::FairClaim { held: 2, claim: 2 })
    );
    assert_eq!(accounting.admit(relay), Ok(()));
}

/// An item leaves exactly at its instant, and a discharge of an item never charged changes
/// nothing.
#[test]
fn test_an_item_leaves_at_its_instant() {
    let mut accounting = SlotAccounting::new(REPLAY);
    let slot = Principal::Slot(7);
    accounting.charge(slot, 20);
    accounting.discharge(slot, 10);
    accounting.advance(19);
    assert_eq!(accounting.held(slot), 1);
    accounting.advance(20);
    assert_eq!(accounting.held(slot), 0);
}

/// The reserved shares: this node, its inbox and its custody each hold a share of their own,
/// apart from the floors and the pool and from each other, so filling one refuses no other.
#[test]
fn test_the_reserved_shares_are_apart() {
    let shares = Shares {
        floor: 0,
        pool: 1,
        local: 1,
        inbox: 1,
        custody: 1,
    };
    let mut accounting = SlotAccounting::new(shares);
    for principal in [Principal::Local, Principal::Inbox, Principal::Custody] {
        assert_eq!(accounting.admit(principal), Ok(()));
        accounting.charge(principal, 9);
        assert_eq!(
            accounting.admit(principal),
            Err(ShareRefusal::ReservedShare { held: 1, share: 1 })
        );
    }
    assert_eq!(accounting.admit(Principal::Slot(0)), Ok(()));
    assert_eq!(accounting.len(), shares.bound(0) - 1);
}
