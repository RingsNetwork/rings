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

use super::ADMISSION_WINDOW_QUANTA;
use super::ADMISSION_WINDOW_QUANTA_WIDE;

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
        // cells lie in the window and the least one lies outside it.
        let cell = next
            .charges
            .iter_mut()
            .min_by_key(|charge| (charge.quantum != quantum, charge.quantum))?;
        *cell = QuantumCharge {
            quantum,
            units: if cell.quantum == quantum {
                cell.units.checked_add(units)?
            } else {
                units
            },
        };
        Some(next)
    }

    /// Whether quantum `r` lies in the window `(s − N, s]`. Quanta after `s` are counted as well,
    /// since only a clock rollback could produce them and the state's clock is monotone anyway.
    fn in_window(charged: u128, quantum: u128) -> bool {
        charged.saturating_add(ADMISSION_WINDOW_QUANTA_WIDE) > quantum
    }
}
