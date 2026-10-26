//! Slot accounting of a node-wide table (load-bounds paper, Law 1, #907).
//!
//! Every item of a table that remote input can grow is charged to the principal that
//! introduced it: the connection slot of the handshake-authenticated edge that delivered it
//! (`fair_admission::slot_roster`), the node itself for its own operations, or one of the
//! node's dedicated duties (its relay inbox, the custody of a range it owns). An origin is never
//! a principal: it is free, while a node has `R` connection slots.
//!
//! Let `f` be the floor per slot, `P` the pool, `L` the local share and `S_d` the share of a
//! dedicated principal `d`; `u(π)` the items of `π`, and for a slot `κ`
//!
//! ```text
//! o(κ) = max(0, u(κ) − f)                    (its pool items)
//! o(υ) = u(υ)                                (a delivery on no slot: pool items alone)
//! Σo   = Σ_κ o(κ) + o(υ)
//! n    = |{π : o(π) > 0}| + [the claimant is not yet counted]
//! ```
//!
//! **Law (admission).** An item introduced by `π` is admitted iff
//!
//! ```text
//! π = κ,  u(κ) < f                         (the slot's floor)
//! π = κ or υ,  Σo < P,  o(π) < ⌈P / n⌉     (the pool, within the fair claim)
//! π = λ,  u(λ) < L                         (the local share)
//! π = d,  u(d) < S_d                       (a dedicated share: μ, the inbox; χ, custody)
//! ```
//!
//! A transition adding several items is admitted iff each is, in order, and refused whole
//! otherwise; so the floor guarantee is per item.
//!
//! and nothing is ever dropped to make room: an item leaves only by
//! [`SlotAccounting::advance`] past its instant or by [`SlotAccounting::discharge`]. An item
//! stays charged to its slot whoever holds the slot, so no transition moves an item into the
//! pool, and
//!
//! ```text
//! |T| ≤ R·f + P + L + S_μ + S_χ       (every transition preserves it)
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
    /// The node itself: its own operations alone.
    Local,
    /// `μ`: deliveries out of this node's own relay inbox (the replay table's inbox share).
    Inbox,
    /// `χ`: the hand-off of a range this node owns, from the node its view names as the
    /// range's previous owner (the removal store's custody share).
    Custody,
    /// A delivery on no slot: on no authenticated edge, or on one retired meanwhile. Every
    /// production connection is bound to its handshake and holds a slot while admitted, so only
    /// a delivery racing its edge's retirement is one; it has no floor, only its fair claim.
    Unbound,
}

/// The shares of one table: the floor per slot, the pool, the local share and the dedicated
/// shares (zero for a duty the table does not serve).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Shares {
    /// `f`: items any slot may hold whatever the others do.
    pub(crate) floor: usize,
    /// `P`: items shared, in fair claims, by over-floor slots.
    pub(crate) pool: usize,
    /// `L`: items of the local principal.
    pub(crate) local: usize,
    /// `S_μ`: items of the inbox principal.
    pub(crate) inbox: usize,
    /// `S_χ`: items of the custody principal.
    pub(crate) custody: usize,
}

impl Shares {
    /// `B(R) = R·f + P + L + S_μ + S_χ`, the bound of a table over `slots` connection slots.
    pub(crate) const fn bound(self, slots: usize) -> usize {
        slots
            .saturating_mul(self.floor)
            .saturating_add(self.pool)
            .saturating_add(self.local)
            .saturating_add(self.inbox)
            .saturating_add(self.custody)
    }

    /// The share of a principal that holds one apart from the floors and the pool: `L`, `S_μ`
    /// or `S_χ`; `None` for a slot or an unbound delivery.
    const fn reserved(self, principal: Principal) -> Option<usize> {
        match principal {
            Principal::Local => Some(self.local),
            Principal::Inbox => Some(self.inbox),
            Principal::Custody => Some(self.custody),
            Principal::Slot(_) | Principal::Unbound => None,
        }
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
    /// A principal with a share of its own (this node, its inbox, its custody) holds it.
    #[error("the principal holds {held} of its share of {share} items")]
    ReservedShare {
        /// `u(π)`.
        held: usize,
        /// `L`, `S_μ` or `S_χ`.
        share: usize,
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
    /// `|{π : o(π) > 0}|`.
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

    /// `o(π)`, the pool items of `principal` (pure); none for a principal with a share of its
    /// own.
    fn pool_items(&self, principal: Principal) -> usize {
        self.pool_items_at(principal, self.held(principal))
    }

    /// The pool items of `principal` were it to hold `held` items (pure).
    fn pool_items_at(&self, principal: Principal, held: usize) -> usize {
        match principal {
            Principal::Slot(_) => held.saturating_sub(self.shares.floor),
            Principal::Unbound => held,
            Principal::Local | Principal::Inbox | Principal::Custody => 0,
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
        self.admit_items(principal, 1)
    }

    /// Whether `items` more items introduced by `principal` are admitted together: each of them
    /// in order, so the transition is admitted whole or refused whole (pure).
    ///
    /// With `o' = o(π)` after the items, `Δ = o' − o(π)` of them fall to the pool, and
    ///
    /// ```text
    /// admit(π, k) ⟺ u(π) + k ≤ S            (a share of its own)
    ///             ∨ Δ = 0                    (within the slot's floor)
    ///             ∨ Σo + Δ ≤ P ∧ o(π) + Δ ≤ ⌈P / n⌉
    /// admit(π, 1) = the admission law, item by item
    /// ```
    ///
    /// `n` counts `π` from its first pool item on, so it is the same for each of the `Δ`.
    pub(crate) fn admit_items(
        &self,
        principal: Principal,
        items: usize,
    ) -> Result<(), ShareRefusal> {
        let held = self.held(principal);
        if let Some(share) = self.shares.reserved(principal) {
            return match held.saturating_add(items) <= share {
                true => Ok(()),
                false => Err(ShareRefusal::ReservedShare { held, share }),
            };
        }
        let claimed = self.pool_items(principal);
        let overflow = self
            .pool_items_at(principal, held.saturating_add(items))
            .saturating_sub(claimed);
        if overflow == 0 {
            return Ok(());
        }
        if self.pool_held.saturating_add(overflow) > self.shares.pool {
            return Err(ShareRefusal::PoolFull {
                held: self.pool_held,
                pool: self.shares.pool,
            });
        }
        let claimants = self.claimants + usize::from(claimed == 0);
        let claim = self.shares.pool.div_ceil(claimants.max(1));
        match claimed.saturating_add(overflow) <= claim {
            true => Ok(()),
            false => Err(ShareRefusal::FairClaim {
                held: claimed,
                claim,
            }),
        }
    }

    /// Whether `items` more items of slot `principal` stay within its floor, which nothing
    /// refuses (pure); a principal that is not a slot has no floor.
    pub(crate) fn within_floor(&self, principal: Principal, items: usize) -> bool {
        matches!(principal, Principal::Slot(_))
            && self.held(principal).saturating_add(items) <= self.shares.floor
    }

    /// Charge `principal` one item that leaves at `until_ms`.
    ///
    /// Pre: [`Self::admit`] admitted it, or it is restored from a store whose items the
    /// admission law put there.
    pub(crate) fn charge(&mut self, principal: Principal, until_ms: u128) {
        self.hold(principal, 1);
        *self.expiry.entry((until_ms, principal)).or_default() += 1;
    }

    /// Charge `principal` `items` items that leave only by [`Self::release`]: a table whose
    /// items leave when their holder drops them, not at an instant.
    ///
    /// Pre: [`Self::admit_items`] admitted them.
    pub(crate) fn hold(&mut self, principal: Principal, items: usize) {
        self.reweigh(principal, |accounting| {
            *accounting.held.entry(principal).or_default() += items;
        });
    }

    /// Forget `items` items of `principal` that [`Self::hold`] charged.
    pub(crate) fn release(&mut self, principal: Principal, items: usize) {
        self.reweigh(principal, |accounting| {
            if let Some(held) = accounting.held.get_mut(&principal) {
                *held = held.saturating_sub(items);
                if *held == 0 {
                    accounting.held.remove(&principal);
                }
            }
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
        self.release(principal, 1);
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
