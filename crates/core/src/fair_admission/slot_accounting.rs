//! Slot accounting of a node-wide table (load-bounds paper, Law 1, #907).
//!
//! Every item of a table that remote input can grow is charged to the principal that
//! introduced it: the connection slot of the handshake-authenticated edge that delivered it
//! (`fair_admission::slot_roster`), or the node itself. An origin is never a principal: it is
//! free, while a node has `R` connection slots.
//!
//! Let `f` be the floor per slot, `P` the pool and `L` the local share; `u(π)` the items of `π`,
//! and for a slot `κ`
//!
//! ```text
//! o(κ) = max(0, u(κ) − f)                    (its pool items)
//! o(υ) = u(υ)                                (a delivery on no slot: pool items alone)
//! Σo   = Σ_κ o(κ) + o(υ)
//! n    = |{π ≠ λ : o(π) > 0}| + [the claimant is not yet counted]
//! ```
//!
//! **Law (admission).** An item introduced by `π` is admitted iff
//!
//! ```text
//! π = κ,  u(κ) < f                         (the slot's floor)
//! π ≠ λ,  Σo < P,  o(π) < ⌈P / n⌉          (the pool, within the fair claim)
//! π = λ,  u(λ) < L                         (the local share)
//! ```
//!
//! and nothing is ever dropped to make room: an item leaves only by
//! [`SlotAccounting::advance`] past its instant or by [`SlotAccounting::discharge`]. An item
//! stays charged to its slot whoever holds the slot, so no transition moves an item into the
//! pool, and
//!
//! ```text
//! |T| ≤ R·f + P + L       (every transition preserves it)
//! ```
//!
//! Every transition costs `O(log |T|)`: the aggregates `Σo` and the count of claimants are kept
//! incrementally, by re-weighing the one principal a transition touches.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

/// The principal that introduced an item of a table.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Principal {
    /// The connection slot of the handshake-authenticated edge that delivered the item.
    Slot(u16),
    /// The node itself: its own operations and deliveries out of its own stores.
    Local,
    /// A delivery on no slot: on no authenticated edge, or on one retired meanwhile. Every
    /// production connection is bound to its handshake and holds a slot while admitted, so only
    /// a delivery racing its edge's retirement is one; it has no floor, only its fair claim.
    Unbound,
}

/// The shares of one table: the floor per slot, the pool and the local share.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Shares {
    /// `f`: items any slot may hold whatever the others do.
    pub(crate) floor: usize,
    /// `P`: items shared, in fair claims, by over-floor slots.
    pub(crate) pool: usize,
    /// `L`: items of the local principal.
    pub(crate) local: usize,
}

impl Shares {
    /// `B(R) = R·f + P + L`, the bound of a table over `slots` connection slots.
    pub(crate) const fn bound(self, slots: usize) -> usize {
        slots
            .saturating_mul(self.floor)
            .saturating_add(self.pool)
            .saturating_add(self.local)
    }
}

/// Why an item was refused, with the figures of the bound it met.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ShareRefusal {
    /// The pool holds `P` items.
    #[error("the pool holds {held} of {pool} items")]
    PoolFull {
        /// `Σo`.
        held: usize,
        /// `P`.
        pool: usize,
    },
    /// The claimant holds its fair claim of the pool.
    #[error("the claimant holds {held} pool items of its fair claim {claim}")]
    FairClaim {
        /// `o(π)`.
        held: usize,
        /// `⌈P / n⌉`.
        claim: usize,
    },
    /// The local principal holds its share.
    #[error("the local principal holds {held} of {local} items")]
    LocalShare {
        /// `u(λ)`.
        held: usize,
        /// `L`.
        local: usize,
    },
    /// The table holds its hard bound, which items no principal is charged for (records that do
    /// not restore) also take.
    #[error("the table holds {held} of {bound} items")]
    TableFull {
        /// The items the table holds.
        held: usize,
        /// Its hard bound.
        bound: usize,
    },
}

/// The accounting of one table (module documentation).
#[derive(Clone, Debug)]
pub(crate) struct SlotAccounting {
    /// The table's shares.
    shares: Shares,
    /// `u(π)` for every principal holding an item.
    held: BTreeMap<Principal, usize>,
    /// The items by the instant they leave and their principal.
    expiry: BTreeMap<(u128, Principal), usize>,
    /// `Σo`.
    pool_held: usize,
    /// `|{π ≠ λ : o(π) > 0}|`.
    claimants: usize,
}

impl SlotAccounting {
    /// An empty table with `shares`.
    pub(crate) fn new(shares: Shares) -> Self {
        Self {
            shares,
            held: BTreeMap::new(),
            expiry: BTreeMap::new(),
            pool_held: 0,
            claimants: 0,
        }
    }

    /// `|T|`, the items held.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.held.values().sum()
    }

    /// `u(π)`.
    pub(crate) fn held(&self, principal: Principal) -> usize {
        self.held.get(&principal).copied().unwrap_or_default()
    }

    /// `o(π)`, the pool items of `principal` (pure); none for the local principal.
    fn pool_items(&self, principal: Principal) -> usize {
        let held = self.held(principal);
        match principal {
            Principal::Slot(_) => held.saturating_sub(self.shares.floor),
            Principal::Unbound => held,
            Principal::Local => 0,
        }
    }

    /// Apply `change` to the state of `principal` alone, re-weighing its pool items.
    fn reweigh(&mut self, principal: Principal, change: impl FnOnce(&mut Self)) {
        let before = self.pool_items(principal);
        change(self);
        let after = self.pool_items(principal);
        // Each aggregate includes `before`, so the subtractions never saturate.
        self.pool_held = self.pool_held.saturating_sub(before) + after;
        self.claimants =
            self.claimants.saturating_sub(usize::from(before > 0)) + usize::from(after > 0);
    }

    /// Whether one more item introduced by `principal` is admitted (the admission law; pure).
    pub(crate) fn admit(&self, principal: Principal) -> Result<(), ShareRefusal> {
        let held = self.held(principal);
        match principal {
            Principal::Local => {
                return match held < self.shares.local {
                    true => Ok(()),
                    false => Err(ShareRefusal::LocalShare {
                        held,
                        local: self.shares.local,
                    }),
                };
            }
            Principal::Slot(_) if held < self.shares.floor => return Ok(()),
            Principal::Slot(_) | Principal::Unbound => {}
        }
        if self.pool_held >= self.shares.pool {
            return Err(ShareRefusal::PoolFull {
                held: self.pool_held,
                pool: self.shares.pool,
            });
        }
        let claimed = self.pool_items(principal);
        let claimants = self.claimants + usize::from(claimed == 0);
        let claim = self.shares.pool.div_ceil(claimants.max(1));
        match claimed < claim {
            true => Ok(()),
            false => Err(ShareRefusal::FairClaim {
                held: claimed,
                claim,
            }),
        }
    }

    /// Charge `principal` one item that leaves at `until_ms`.
    ///
    /// Pre: [`Self::admit`] admitted it, or it is restored from a store whose items the
    /// admission law put there.
    pub(crate) fn charge(&mut self, principal: Principal, until_ms: u128) {
        self.reweigh(principal, |accounting| {
            *accounting.held.entry(principal).or_default() += 1;
            *accounting.expiry.entry((until_ms, principal)).or_default() += 1;
        });
    }

    /// Forget one item of `principal` that leaves at `until_ms`; nothing when it holds none.
    pub(crate) fn discharge(&mut self, principal: Principal, until_ms: u128) {
        let Some(count) = self.expiry.get_mut(&(until_ms, principal)) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            self.expiry.remove(&(until_ms, principal));
        }
        self.reweigh(principal, |accounting| {
            if let Some(held) = accounting.held.get_mut(&principal) {
                *held = held.saturating_sub(1);
                if *held == 0 {
                    accounting.held.remove(&principal);
                }
            }
        });
    }

    /// Forget every item whose instant is at or before `now_ms`.
    pub(crate) fn advance(&mut self, now_ms: u128) {
        while let Some(((until_ms, principal), _)) = self.expiry.first_key_value() {
            if *until_ms > now_ms {
                return;
            }
            let (until_ms, principal) = (*until_ms, *principal);
            self.discharge(principal, until_ms);
        }
    }
}

#[cfg(test)]
mod tests;
