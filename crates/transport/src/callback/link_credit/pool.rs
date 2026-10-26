//! The node's frame pool: the credit a node lends its connections' data lanes beyond their
//! floors (load-bounds paper, Law 3, #934).
//!
//! ```text
//!   used = Σ charge(e, ℓ) ≤ P_F         over every data lane of every connection
//!   demand(e, ℓ)  ─▶ FIFO ─serve─▶ lend(e, ℓ, min(want, P_F − used))
//!   adjust(before, after) ─ after < before ─▶ serve
//! ```
//!
//! A lane's charge is what it holds of the pool: its share, or the liability it still counts
//! beyond its floor while a shrink is pending or its frames drain, whichever is greater (see
//! `ReceiveWindow::charge`). Each link reports every change of its lanes' charges, so `used` is
//! their sum; and a share is lent only from what is free, so `used ≤ P_F` always, whatever the
//! number of connections. With every lane at most `W` and the priority lane never pooled,
//!
//! ```text
//! Σ liab ≤ R·(W + 3·g_data) + P_F = B_F
//! ```
//!
//! Demands are served first come, first served: a lane whose demand waits is served before a
//! later one, and served in part when the pool frees less than it wants, keeping its place.
//!
//! Lock law: the pool's lock is never held together with a link's, nor across a suspension
//! point; a link reports and lends outside its own lock.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;

use super::LinkCredit;
use crate::core::credit::CreditIndex;
use crate::sync_utils::lock_recover;

/// `P_F`, the frames a node lends its data lanes beyond their floors: 256 frames, 16 MiB at the
/// data-channel ceiling, the node-wide budget the transport held before per-lane credit.
pub(crate) const NODE_FRAME_POOL: u64 = 256;

/// One lane waiting for the pool.
struct Demand {
    /// Its connection, held weakly: a demand keeps no connection alive.
    link: Weak<LinkCredit>,
    /// The lane.
    index: CreditIndex,
}

impl Demand {
    /// Whether this is the demand of `index` of `link`.
    fn is_of(&self, link: &Weak<LinkCredit>, index: CreditIndex) -> bool {
        self.index == index && Weak::ptr_eq(&self.link, link)
    }
}

/// What [`NodeFramePool`] keeps under its lock.
struct PoolState {
    /// `Σ charge`, the frames lent or still counted beyond the floors.
    used: u64,
    /// `P_F`.
    capacity: u64,
    /// The lanes waiting for the pool, in arrival order.
    demands: VecDeque<Demand>,
}

/// The node's frame pool (module documentation): one per transport, shared by its connections.
///
/// Clone law: clones name the same pool.
#[derive(Clone)]
pub struct NodeFramePool(Arc<Mutex<PoolState>>);

impl NodeFramePool {
    /// An empty pool of the production capacity, `NODE_FRAME_POOL` (256 frames).
    pub fn new() -> Self {
        Self::with_capacity(NODE_FRAME_POOL)
    }

    /// An empty pool of `capacity` frames.
    pub(crate) fn with_capacity(capacity: u64) -> Self {
        Self(Arc::new(Mutex::new(PoolState {
            used: 0,
            capacity,
            demands: VecDeque::new(),
        })))
    }

    /// The state, for one transition.
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        lock_recover(&self.0)
    }

    /// A lane's charge changed from `before` to `after`: count it, and serve the waiting
    /// demands with what it freed.
    pub(super) fn adjust(&self, before: u64, after: u64) {
        if before == after {
            return;
        }
        {
            let mut state = self.lock();
            state.used = state.used.saturating_sub(before).saturating_add(after);
        }
        if after < before {
            self.serve();
        }
    }

    /// Queue the demand of `index` of `link`, unless it waits already, and serve.
    pub(super) fn demand(&self, link: &Arc<LinkCredit>, index: CreditIndex) {
        let link = Arc::downgrade(link);
        {
            let mut state = self.lock();
            if !state
                .demands
                .iter()
                .any(|demand| demand.is_of(&link, index))
            {
                state.demands.push_back(Demand { link, index });
            }
        }
        self.serve();
    }

    /// Serve the waiting demands in order, while the pool has room: each is lent what it wants,
    /// or what is free, and keeps its place while it wants more. The share is counted under the
    /// pool's lock and handed to the link outside it; a link that takes less gives the rest back
    /// through [`Self::adjust`].
    fn serve(&self) {
        loop {
            let Some((link, index)) = self
                .lock()
                .demands
                .front()
                .map(|demand| (demand.link.clone(), demand.index))
            else {
                return;
            };
            let want = link.upgrade().map_or(0, |link| link.want(index));
            let lent = {
                let mut state = self.lock();
                let free = state.capacity.saturating_sub(state.used);
                let lent = want.min(free);
                state.used = state.used.saturating_add(lent);
                if lent == want
                    && state
                        .demands
                        .front()
                        .is_some_and(|demand| demand.is_of(&link, index))
                {
                    state.demands.pop_front();
                }
                lent
            };
            match link.upgrade() {
                Some(link) if lent > 0 => link.lend(index, lent),
                Some(_) => {}
                None => self.adjust(lent, 0),
            }
            if lent < want {
                return;
            }
        }
    }

    /// Forget every demand of `link`, which is gone.
    pub(super) fn forget(&self, link: &Weak<LinkCredit>) {
        self.lock()
            .demands
            .retain(|demand| !Weak::ptr_eq(&demand.link, link));
    }

    /// `Σ charge`, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn used(&self) -> u64 {
        self.lock().used
    }

    /// The lanes waiting for the pool, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn waiting(&self) -> usize {
        self.lock().demands.len()
    }
}

impl Default for NodeFramePool {
    fn default() -> Self {
        Self::new()
    }
}
