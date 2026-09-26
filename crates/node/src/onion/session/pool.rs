//! The reply-block pool `Q_{h,ς}` of one session at its world-facing hop `h` (#834 D8,
//! Invariant Credit).
//!
//! ```text
//! add(υ, t)  : Q ← purge_t(Q) ∪ {υ}     if |purge_t(Q)| < Q_max, else υ is dropped
//! take(t)    : υ = argmin_{υ ∈ purge_t(Q)} x_υ;  Q ← purge_t(Q) ∖ {υ}
//! purge_t(Q) = { υ ∈ Q | t < x_υ }
//! ```
//!
//! Laws (tested in `session::tests`):
//!
//! - **Single use.** A block leaves the pool when it is taken, before its reply is produced, and
//!   is never returned to it, so no block produces two replies.
//! - **Bound.** `|Q| ≤ Q_max` after every step; credit beyond it is dropped (D8).
//! - **Expiry.** No block with `x ≤ t` is ever taken at `t`, and a purge drops exactly those.
//! - **Order.** `take` returns a block of least `x`, so the blocks nearest to expiry are spent
//!   first; ties are broken by arrival, first in first out.

use std::collections::BTreeMap;

use crate::onion::circuit::OnionExpiry;
use crate::onion::sphinx::cell::OnionSurb;
use crate::onion::sphinx::class::OnionLoopClass;

/// `Q_max = 256`, the most reply blocks one session's pool holds (D8): `256 · 2979 B ≈ 763 KB`.
pub(crate) const ONION_SURB_POOL_CAPACITY: usize = 256;

/// The reply-block pool of one session; see the module documentation.
#[derive(Debug, Default)]
pub(crate) struct OnionSurbPool {
    /// The blocks, keyed by `(x, arrival index)`: least expiry first, arrival order within one.
    surbs: BTreeMap<(OnionExpiry, u64), OnionSurb>,
    /// The next arrival index.
    arrivals: u64,
}

impl OnionSurbPool {
    /// `|Q|` at `now`, after the purge.
    pub(crate) fn len(&mut self, now_ms: u128) -> usize {
        self.purge(now_ms);
        self.surbs.len()
    }

    /// Whether the pool holds no live block at `now`: the session then stops reading the world
    /// (Invariant Credit).
    pub(crate) fn is_empty(&mut self, now_ms: u128) -> bool {
        self.len(now_ms) == 0
    }

    /// The class of the block [`Self::take`] would return at `now`, which bounds the reply it
    /// can carry; `None` for an empty pool.
    pub(crate) fn least_class(&mut self, now_ms: u128) -> Option<OnionLoopClass> {
        self.purge(now_ms);
        self.surbs.first_key_value().map(|(_, surb)| surb.class())
    }

    /// Add one block at `now`, returning whether the pool kept it: a block whose expiry is not
    /// admissible now (passed, or beyond `now + V`, so its reply would be refused by the first
    /// relay), or credit beyond `Q_max`, is dropped (D8).
    pub(crate) fn add(&mut self, now_ms: u128, surb: OnionSurb) -> bool {
        self.purge(now_ms);
        if !surb.expiry().admissible_at(now_ms) || self.surbs.len() >= ONION_SURB_POOL_CAPACITY {
            return false;
        }
        let arrival = self.arrivals;
        self.arrivals = self.arrivals.wrapping_add(1);
        self.surbs.insert((surb.expiry(), arrival), surb);
        true
    }

    /// Take the live block of least expiry at `now`, removing it from the pool.
    pub(crate) fn take(&mut self, now_ms: u128) -> Option<OnionSurb> {
        self.purge(now_ms);
        self.surbs.pop_first().map(|(_, surb)| surb)
    }

    /// Drop every block whose `x` has passed at `now`.
    fn purge(&mut self, now_ms: u128) {
        while self
            .surbs
            .first_key_value()
            .is_some_and(|((expiry, _), _)| expiry.has_passed_at(now_ms))
        {
            self.surbs.pop_first();
        }
    }
}
