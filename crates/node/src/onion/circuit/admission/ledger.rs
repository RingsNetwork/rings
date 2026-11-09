//! Unit ledger over the sliding window of `V / Q` aligned arrival quanta (L9 budgets).
//!
//! # Model
//!
//! Let `q(t) = ⌊t / Q⌋` be the arrival quantum of instant `t`, and let `N = V / Q = 5`. A ledger
//! is a function `c : ℕ → ℕ` from quanta to charged units with support in the last `N` quanta.
//! Its load at quantum `s` is the sum over the window:
//!
//! ```text
//! load_s(c) = Σ_{s − N < r ≤ s} c(r)
//! ```
//!
//! A charge of `u` units at `s` under cap `C` is the partial map
//! `charge_s(c, u) = c[s ↦ c(s) + u]` if `load_s(c) + u ≤ C`, and `⊥` otherwise.
//!
//! # Laws
//!
//! * Window bound: for every quantum `s`, `Σ_{s − N < r ≤ s} c(r) ≤ C`. Charges happen only at
//!   the current quantum, and each charge checks the full window that ends there.
//! * Finite support: the ledger stores `N` cells. A charge at `s` overwrites only a cell whose
//!   quantum is `≤ s − N`. Under a monotone clock such a cell has already left every future
//!   window, so it cannot be forgotten early.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use rings_core::dht::Did;

use super::ADMISSION_WINDOW_QUANTA;
use super::ADMISSION_WINDOW_QUANTA_WIDE;

/// Identifier of one connection slot `0..2·R` in [`super::OnionAdmissionState`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OnionSlotId(usize);

impl OnionSlotId {
    /// Wrap a zero-based slot index.
    pub(crate) const fn new(index: usize) -> Self {
        Self(index)
    }

    /// The zero-based slot index.
    pub(crate) const fn index(self) -> usize {
        self.0
    }
}

/// Units charged in one arrival quantum.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct QuantumCharge {
    /// Arrival quantum `r = ⌊arr / Q⌋`.
    quantum: u128,
    /// Units `c(r)` charged in that quantum.
    units: u32,
}

/// A unit ledger with support in the last `N = V / Q` arrival quanta.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct QuantumLedger {
    /// One cell per quantum of the window; stale cells are reused in place.
    charges: [QuantumCharge; ADMISSION_WINDOW_QUANTA],
}

impl QuantumLedger {
    /// `load_s(c)`: the units charged in the window `(s − N, s]`.
    pub(super) fn load(&self, quantum: u128) -> u32 {
        self.charges
            .iter()
            .filter(|charge| Self::in_window(charge.quantum, quantum))
            .fold(0_u32, |load, charge| load.saturating_add(charge.units))
    }

    /// `charge_s(c, u)`: the ledger with `units` more at `quantum`, or `None` if that would exceed
    /// `cap` over the window. The input ledger is unchanged, so a caller can check every budget
    /// before committing any of them.
    pub(super) fn charged(self, quantum: u128, units: u32, cap: u32) -> Option<Self> {
        self.load(quantum)
            .checked_add(units)
            .filter(|load| *load <= cap)?;
        let mut next = self;
        // The cell of `s` if it exists, else the cell with the least quantum. The window
        // (s − N, s] holds at most N distinct quanta, so when `s` has no cell at most N − 1
        // cells lie in the window and the least one lies outside it. The array is non-empty, so
        // the fold from its head is total.
        let [head, tail @ ..] = &mut next.charges;
        let cell = tail.iter_mut().fold(head, |best, charge| {
            if (charge.quantum != quantum, charge.quantum) < (best.quantum != quantum, best.quantum)
            {
                charge
            } else {
                best
            }
        });
        // When `cell.quantum = s`, `cell.units + units ≤ load_s + units ≤ cap`, so saturation
        // never engages: the sum is exact.
        let charged = if cell.quantum == quantum {
            cell.units.saturating_add(units)
        } else {
            units
        };
        *cell = QuantumCharge {
            quantum,
            units: charged,
        };
        Some(next)
    }

    /// Whether quantum `r` lies in the window `(s − N, s]`. Quanta after `s` are counted as well,
    /// since only a clock rollback could produce them and the state's clock is monotone anyway.
    pub(super) fn in_window(charged: u128, quantum: u128) -> bool {
        charged.saturating_add(ADMISSION_WINDOW_QUANTA_WIDE) > quantum
    }
}

/// One connection slot's unit ledger, the live link generations currently occupying it, and any
/// DIDs whose own prior charges remain in `ledger` while the window drains.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct SlotLedger {
    /// Units charged to this connection slot in the window.
    pub(super) ledger: QuantumLedger,
    /// The live link generations of each DID currently occupying this slot.
    pub(super) live: BTreeMap<Did, BTreeSet<u64>>,
    /// DIDs that have charged units on this slot, mapped to the latest quantum they were charged.
    pub(super) retained: BTreeMap<Did, u128>,
}

impl SlotLedger {
    /// Whether no live link currently occupies this slot.
    pub(super) fn is_free(&self) -> bool {
        self.live.is_empty()
    }

    /// Whether the slot ledger may be released at `quantum`: it has no live link and no load.
    pub(super) fn is_releasable_at(&self, quantum: u128) -> bool {
        self.live.is_empty() && self.ledger.load(quantum) == 0
    }

    /// Whether `did` holds `generation` as a live link on this slot.
    pub(super) fn contains_live(&self, did: Did, generation: u64) -> bool {
        self.live
            .get(&did)
            .is_some_and(|generations| generations.contains(&generation))
    }

    /// Whether `did` has an unexpired charge of its own on this slot at `quantum`.
    pub(super) fn has_unexpired_charge(&self, did: Did, quantum: u128) -> bool {
        self.ledger.load(quantum) > 0
            && self
                .retained
                .get(&did)
                .is_some_and(|&charged_quantum| QuantumLedger::in_window(charged_quantum, quantum))
    }

    /// The number of live link generations on this slot.
    pub(super) fn live_link_count(&self) -> usize {
        self.live.values().map(BTreeSet::len).sum()
    }

    /// Remove `generation` from `did`'s live set on this slot, returning `Some(true)` when
    /// `did`'s last live generation on this slot was removed, `Some(false)` when `did` still
    /// holds another live generation, or `None` when `(did, generation)` was not live.
    pub(super) fn remove_live_generation(&mut self, did: Did, generation: u64) -> Option<bool> {
        let (removed, empty) = {
            let generations = self.live.get_mut(&did)?;
            (generations.remove(&generation), generations.is_empty())
        };
        removed.then(|| {
            if empty {
                self.live.remove(&did);
            }
            empty
        })
    }

    /// Take the retained DIDs whose window since retirement has elapsed (or whose slot load has
    /// reached zero) at `quantum` and that are not currently live on this slot.
    pub(super) fn take_drained_retained(&mut self, quantum: u128) -> Vec<Did> {
        let retained = std::mem::take(&mut self.retained);
        let (kept, drained) = match self.ledger.load(quantum) == 0 {
            true => (BTreeMap::new(), retained),
            false => retained.into_iter().partition(|&(_, retired_quantum)| {
                QuantumLedger::in_window(retired_quantum, quantum)
            }),
        };
        self.retained = kept;
        drained
            .into_keys()
            .filter(|did| !self.live.contains_key(did))
            .collect()
    }
}
