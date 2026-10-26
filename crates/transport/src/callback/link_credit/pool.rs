//! The node's frame pool: the credit a node lends its connections' data lanes beyond their
//! floors (load-bounds paper, Law 3, #934).
//!
//! ```text
//!   p_e   = Σ_ℓ charge(e, ℓ)                       the pooled frames of edge e
//!   n     = |{e : p_e > 0 or e waits}|             the claimants
//!   claim = ⌈P_F / n⌉                              Law 1's fair claim
//!   share = ⌊P_F / n⌋                              what every claimant is guaranteed
//!   used  = Σ_e p_e ≤ P_F
//!
//!   demand(e, ℓ) ─▶ FIFO ─serve─▶ lend(e, ℓ, min(want, claim − p_e, P_F − used))
//!   adjust(e, before, after) ─ after < before ─▶ serve
//!   a demand of an e with p_e < share finds the pool full ─▶ every e′ with p_e′ > share
//!                                                            shrinks to it
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
//! Demands are served first come, first served within the claims: a demand at its edge's
//! claim keeps its place without holding back a later one, and a demand served in part keeps
//! its place until the pool frees more. An edge above its share keeps what it holds until a
//! claimant below its share finds the pool full; then it is shrunk to its share, so a busy edge
//! holds at least `min(3·(W − 1), ⌊P_F / n⌋)` pooled frames within two round trips
//! (Prop. 3(5)): the shares fit the pool, `n·⌊P_F / n⌋ ≤ P_F`, as the claims need not. An edge
//! at its share never shrinks another, so a pool shared out at the shares stays put.
//!
//! Lock law: the pool's lock is never held together with a link's, nor across a suspension
//! point; a link reports, lends and shrinks outside the pool's lock.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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

/// The identity of an edge in the pool: a number the pool gives each link it serves, once, in
/// increasing order, so no two links, alive or gone, ever share one.
pub(super) type EdgeKey = u64;

/// One lane waiting for the pool.
struct Demand {
    /// Its connection, held weakly: a demand keeps no connection alive.
    link: Weak<LinkCredit>,
    /// Its edge.
    edge: EdgeKey,
    /// The lane.
    index: CreditIndex,
}

impl Demand {
    /// The key of the demand's edge.
    const fn edge(&self) -> EdgeKey {
        self.edge
    }

    /// Whether this is the demand of `index` of the edge `edge`.
    fn is_of(&self, edge: EdgeKey, index: CreditIndex) -> bool {
        self.index == index && self.edge() == edge
    }
}

/// One edge holding pooled credit.
struct EdgeShare {
    /// The edge, held weakly, to shrink it when a claimant waits.
    link: Weak<LinkCredit>,
    /// `p_e`, the pooled frames it holds.
    held: u64,
}

/// What [`NodeFramePool`] keeps under its lock.
struct PoolState {
    /// `Σ p_e`, the frames lent or still counted beyond the floors.
    used: u64,
    /// `P_F`.
    capacity: u64,
    /// The edges holding pooled credit.
    edges: BTreeMap<EdgeKey, EdgeShare>,
    /// The lanes waiting for the pool, in arrival order.
    demands: VecDeque<Demand>,
    /// The key the next link registered gets.
    next_edge: EdgeKey,
}

impl PoolState {
    /// `p_e`.
    fn held(&self, edge: EdgeKey) -> u64 {
        self.edges.get(&edge).map_or(0, |share| share.held)
    }

    /// `n`, the edges that hold pooled credit or wait for it, at least one (pure).
    fn claimants(&self) -> u64 {
        let claimants = self
            .edges
            .keys()
            .copied()
            .chain(self.demands.iter().map(Demand::edge))
            .collect::<BTreeSet<_>>()
            .len();
        u64::try_from(claimants).unwrap_or(u64::MAX).max(1)
    }

    /// `⌈P_F / n⌉`, the fair claim an edge is lent within (Law 1; pure).
    fn claim(&self) -> u64 {
        self.capacity.div_ceil(self.claimants())
    }

    /// `⌊P_F / n⌋`, the share every claimant is guaranteed (pure).
    fn share(&self) -> u64 {
        self.capacity / self.claimants()
    }

    /// Count `p_e ← p_e − before + after` for `edge`; `link` names the edge on its first share.
    fn count(&mut self, (edge, link): (EdgeKey, &Weak<LinkCredit>), (before, after): (u64, u64)) {
        let share = self.edges.entry(edge).or_insert_with(|| EdgeShare {
            link: link.clone(),
            held: 0,
        });
        share.held = share.held.saturating_sub(before).saturating_add(after);
        if share.held == 0 {
            self.edges.remove(&edge);
        }
        self.used = self.used.saturating_sub(before).saturating_add(after);
    }
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
            edges: BTreeMap::new(),
            demands: VecDeque::new(),
            next_edge: 0,
        })))
    }

    /// Register a link: the key of its edge, never given to another.
    pub(super) fn register(&self) -> EdgeKey {
        let mut state = self.lock();
        let edge = state.next_edge;
        state.next_edge = edge.saturating_add(1);
        edge
    }

    /// The link of the edge `edge` is gone: whatever the pool still counts for it, lent or
    /// draining, and every demand of it leave the pool, which serves what that frees. Total:
    /// called on every path a link ends by (its drop), so no edge outlives its link.
    pub(super) fn release(&self, edge: EdgeKey) {
        {
            let mut state = self.lock();
            if let Some(share) = state.edges.remove(&edge) {
                state.used = state.used.saturating_sub(share.held);
            }
            state.demands.retain(|demand| demand.edge != edge);
        }
        self.serve();
    }

    /// The state, for one transition.
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        lock_recover(&self.0)
    }

    /// A lane of the edge `edge` changed its charge from `before` to `after`: count it, and
    /// serve the waiting demands with what it freed. A charge only grows by a share the pool
    /// lent, which named the edge, so an edge this names first holds nothing to shrink.
    pub(super) fn adjust(&self, edge: EdgeKey, before: u64, after: u64) {
        if before == after {
            return;
        }
        self.lock().count((edge, &Weak::new()), (before, after));
        if after < before {
            self.serve();
        }
    }

    /// Queue the demand of `index` of `link`, unless it waits already, and serve.
    pub(super) fn demand(&self, link: &Arc<LinkCredit>, index: CreditIndex) {
        let edge = link.edge();
        {
            let mut state = self.lock();
            if !state.demands.iter().any(|demand| demand.is_of(edge, index)) {
                state.demands.push_back(Demand {
                    link: Arc::downgrade(link),
                    edge,
                    index,
                });
            }
        }
        self.serve();
    }

    /// Serve the waiting demands in order, while the pool has room: each is lent what it
    /// wants, within its edge's claim and what is free, and keeps its place while it wants more;
    /// a demand at its claim is passed over. The share is counted under the pool's lock and
    /// handed to the link outside it; a link that takes less gives the rest back through
    /// [`Self::adjust`]. When a demand of an edge below its share finds the pool full, every
    /// edge above its share is shrunk to it.
    fn serve(&self) {
        let mut position = 0;
        loop {
            let Some((link, edge, index)) = self
                .lock()
                .demands
                .get(position)
                .map(|demand| (demand.link.clone(), demand.edge, demand.index))
            else {
                return;
            };
            let want = link.upgrade().map_or(0, |link| link.want(index));
            let (lent, starved) = {
                let mut state = self.lock();
                let room = state.claim().saturating_sub(state.held(edge));
                let free = state.capacity.saturating_sub(state.used);
                let lent = want.min(room).min(free);
                if lent > 0 {
                    state.count((edge, &link), (0, lent));
                }
                let current = state
                    .demands
                    .get(position)
                    .is_some_and(|demand| demand.is_of(edge, index));
                if lent == want && current {
                    state.demands.remove(position);
                } else {
                    position += 1;
                }
                let starved = lent < want.min(room) && state.held(edge) < state.share();
                (lent, starved)
            };
            match link.upgrade() {
                Some(link) if lent > 0 => link.lend(index, lent),
                Some(_) => {}
                None => self.lock().count((edge, &link), (lent, 0)),
            }
            if starved {
                self.rebalance();
                return;
            }
        }
    }

    /// Shrink every edge above its share to it, since a claimant below its share found the
    /// pool full. The shrinks take effect at their returns, which give the excess back.
    fn rebalance(&self) {
        let over = {
            let state = self.lock();
            let share = state.share();
            state
                .edges
                .values()
                .filter(|edge| edge.held > share)
                .map(|edge| (edge.link.clone(), share))
                .collect::<Vec<_>>()
        };
        for (link, share) in over {
            if let Some(link) = link.upgrade() {
                link.shrink_to_share(share);
            }
        }
    }

    /// Forget every demand of `link`, which closed.
    pub(super) fn forget(&self, link: &LinkCredit) {
        let edge = link.edge();
        self.lock().demands.retain(|demand| demand.edge != edge);
    }

    /// `Σ p_e`, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn used(&self) -> u64 {
        self.lock().used
    }

    /// `p_e` of `link`, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn held_by(&self, link: &LinkCredit) -> u64 {
        self.lock().held(link.edge())
    }

    /// The edges holding pooled credit, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn edges(&self) -> usize {
        self.lock().edges.len()
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
