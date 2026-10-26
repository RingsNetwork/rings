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
//!   demand(e, ℓ) ─▶ FIFO below the shares, then below the claims
//!               ─serve─▶ lend(e, ℓ, min(want, claim − p_e, P_F − used))
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
//! Demands are served first come, first served, those of edges below their share before those
//! of edges between their share and their claim: a demand at its edge's claim keeps its place
//! without holding back a later one, and a demand served in part keeps its place until the pool
//! frees more. An edge above its share keeps what it holds until a
//! claimant below its share finds the pool full; then it is shrunk to its share, so a busy edge
//! holds at least `min(3·(W − 1), ⌊P_F / n⌋)` pooled frames within two round trips
//! (Prop. 3(5)): the shares fit the pool, `n·⌊P_F / n⌋ ≤ P_F`, as the claims need not. An edge
//! at its share never shrinks another, so a pool shared out at the shares stays put.
//!
//! Cost (load-bounds paper, Obligation "Admission complexity", #938). An edge has an entry
//! exactly while it holds pooled credit or waits, so `n` is the number of entries. Edges that
//! wait are indexed by what they hold, each under its earliest demand, so the next demand to
//! serve is the earliest among the edges holding less than the claim: holdings are integers,
//! and only the holdings some waiting edge has are visited, at most `min(n, ⌈P_F / n⌉) ≤ √P_F`
//! of them, so each demand served costs `O(log n)`. A rebalance reads the edges above their
//! share off the same index of holdings.
//!
//! Lock law: the pool's lock is never held together with a link's, nor across a suspension
//! point; a link reports, lends and shrinks outside the pool's lock.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
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

/// The position of a demand in arrival order.
type Arrival = u64;

/// One edge that holds pooled credit or waits for it.
struct EdgeShare {
    /// The edge, held weakly, to lend to it and to shrink it.
    link: Weak<LinkCredit>,
    /// `p_e`, the pooled frames it holds.
    held: u64,
    /// Its lanes waiting for the pool, by arrival.
    demands: BTreeMap<Arrival, CreditIndex>,
}

impl EdgeShare {
    /// Whether the edge is a claimant: it holds pooled credit or waits.
    fn claims(&self) -> bool {
        self.held > 0 || !self.demands.is_empty()
    }

    /// The edge's earliest demand.
    fn head(&self) -> Option<(Arrival, CreditIndex)> {
        self.demands
            .first_key_value()
            .map(|(arrival, index)| (*arrival, *index))
    }
}

/// What [`NodeFramePool`] keeps under its lock.
struct PoolState {
    /// `Σ p_e`, the frames lent or still counted beyond the floors.
    used: u64,
    /// `P_F`.
    capacity: u64,
    /// The claimants: every edge that holds pooled credit or waits, and only those.
    edges: BTreeMap<EdgeKey, EdgeShare>,
    /// The claimants by what they hold, for a rebalance.
    by_held: BTreeMap<u64, BTreeSet<EdgeKey>>,
    /// The waiting claimants by what they hold, each under its earliest demand.
    waiting: BTreeMap<u64, BTreeSet<(Arrival, EdgeKey)>>,
    /// The arrival the next demand gets.
    next_arrival: Arrival,
    /// The key the next link registered gets.
    next_edge: EdgeKey,
}

impl PoolState {
    /// `p_e`.
    fn held(&self, edge: EdgeKey) -> u64 {
        self.edges.get(&edge).map_or(0, |share| share.held)
    }

    /// `n`, the claimants, at least one (pure).
    fn claimants(&self) -> u64 {
        u64::try_from(self.edges.len()).unwrap_or(u64::MAX).max(1)
    }

    /// `⌈P_F / n⌉`, the fair claim an edge is lent within (Law 1; pure).
    fn claim(&self) -> u64 {
        self.capacity.div_ceil(self.claimants())
    }

    /// `⌊P_F / n⌋`, the share every claimant is guaranteed (pure).
    fn share(&self) -> u64 {
        self.capacity / self.claimants()
    }

    /// Change the entry of `edge` by `change`, keeping both indices and the claimant set
    /// consistent: the entry exists exactly while the edge claims. `link` names the edge when
    /// it has no entry yet.
    fn update(
        &mut self,
        (edge, link): (EdgeKey, &Weak<LinkCredit>),
        change: impl FnOnce(&mut EdgeShare),
    ) {
        let mut share = self.edges.remove(&edge).unwrap_or_else(|| EdgeShare {
            link: link.clone(),
            held: 0,
            demands: BTreeMap::new(),
        });
        self.unindex(edge, &share);
        change(&mut share);
        if share.claims() {
            self.index(edge, &share);
            self.edges.insert(edge, share);
        }
    }

    /// Enter `share` of `edge` in the indices.
    fn index(&mut self, edge: EdgeKey, share: &EdgeShare) {
        self.by_held.entry(share.held).or_default().insert(edge);
        if let Some((arrival, _)) = share.head() {
            self.waiting
                .entry(share.held)
                .or_default()
                .insert((arrival, edge));
        }
    }

    /// Take `share` of `edge` out of the indices.
    fn unindex(&mut self, edge: EdgeKey, share: &EdgeShare) {
        remove_from(&mut self.by_held, share.held, &edge);
        if let Some((arrival, _)) = share.head() {
            remove_from(&mut self.waiting, share.held, &(arrival, edge));
        }
    }

    /// Count `p_e ← p_e − before + after` for `edge`.
    fn count(&mut self, (edge, link): (EdgeKey, &Weak<LinkCredit>), (before, after): (u64, u64)) {
        self.update((edge, link), |share| {
            share.held = share.held.saturating_sub(before).saturating_add(after);
        });
        self.used = self.used.saturating_sub(before).saturating_add(after);
    }

    /// The next demand to serve, with the holdings visited to find it (pure): the earliest of
    /// an edge holding less than its share, or else the earliest of an edge holding less than
    /// its claim. Shares come first, so the frames a rebalance frees reach the edges it freed
    /// them for, not an edge already at its share.
    fn next_servable(&self) -> (Option<(EdgeKey, CreditIndex)>, u64) {
        let mut visited = 0_u64;
        let mut earliest = |below: std::ops::Range<u64>| {
            self.waiting
                .range(below)
                .inspect(|_| visited = visited.saturating_add(1))
                .filter_map(|(_, heads)| heads.first())
                .min()
                .copied()
        };
        let (share, claim) = (self.share(), self.claim());
        let earliest = earliest(0..share).or_else(|| earliest(share..claim));
        let next = earliest.and_then(|(arrival, edge)| {
            let index = self.edges.get(&edge)?.demands.get(&arrival)?;
            Some((edge, *index))
        });
        (next, visited)
    }
}

/// Remove `value` from the set under `key` of `index`, and the set once it is empty.
fn remove_from<K: Ord + Copy, V: Ord>(index: &mut BTreeMap<K, BTreeSet<V>>, key: K, value: &V) {
    if let Some(set) = index.get_mut(&key) {
        set.remove(value);
        if set.is_empty() {
            index.remove(&key);
        }
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
            by_held: BTreeMap::new(),
            waiting: BTreeMap::new(),
            next_arrival: 0,
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
                state.unindex(edge, &share);
                state.used = state.used.saturating_sub(share.held);
            }
        }
        self.serve();
    }

    /// The state, for one transition.
    fn lock(&self) -> MutexGuard<'_, PoolState> {
        #[cfg(feature = "dummy")]
        crate::hot_path::lock_taken();
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
        {
            let mut state = self.lock();
            let edge = link.edge();
            let queued = state
                .edges
                .get(&edge)
                .is_some_and(|share| share.demands.values().any(|queued| *queued == index));
            if !queued {
                let arrival = state.next_arrival;
                state.next_arrival = arrival.saturating_add(1);
                state.update((edge, &Arc::downgrade(link)), |share| {
                    share.demands.insert(arrival, index);
                });
            }
        }
        self.serve();
    }

    /// Serve the waiting demands in arrival order, those of edges below their share first, then
    /// those of edges below their claim, while the pool has room: each is lent what it wants,
    /// within its edge's claim and what is free; a demand served in part keeps its place, and an
    /// edge that reaches its claim leaves the servable ones. The share is counted under the
    /// pool's lock and handed to the link outside it; a link that takes less gives the rest back
    /// through [`Self::adjust`]. When a demand of an edge below its share finds the pool full,
    /// every edge above its share is shrunk to it.
    fn serve(&self) {
        #[cfg(feature = "dummy")]
        crate::hot_path::record(crate::hot_path::HotPath::PoolService, 0);
        #[cfg(feature = "dummy")]
        let _service = crate::hot_path::service();
        loop {
            let (next, link) = {
                let state = self.lock();
                let (next, visited) = state.next_servable();
                #[cfg(feature = "dummy")]
                crate::hot_path::charge(crate::hot_path::HotPath::PoolService, visited);
                #[cfg(not(feature = "dummy"))]
                let _ = visited;
                let Some((edge, index)) = next else {
                    return;
                };
                let link = state.edges.get(&edge).map(|share| share.link.clone());
                ((edge, index), link.unwrap_or_default())
            };
            let (edge, index) = next;
            let want = link.upgrade().map_or(0, |link| link.want(index));
            let (lent, starved, full) = {
                let mut state = self.lock();
                let room = state.claim().saturating_sub(state.held(edge));
                let free = state.capacity.saturating_sub(state.used);
                let lent = want.min(room).min(free);
                state.count((edge, &link), (0, lent));
                if lent == want {
                    state.update((edge, &link), |share| {
                        share.demands.retain(|_, queued| *queued != index);
                    });
                }
                let starved = lent < want.min(room) && state.held(edge) < state.share();
                (lent, starved, lent == free)
            };
            match link.upgrade() {
                Some(link) => {
                    if lent > 0 {
                        link.lend(index, lent);
                    }
                    if lent == want {
                        link.served(index);
                    }
                }
                None => self.lock().count((edge, &link), (lent, 0)),
            }
            if starved {
                self.rebalance();
            }
            // A demand cut short by its edge's claim leaves the servable ones; one cut short by
            // a full pool ends the service.
            if lent < want && full {
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
                .by_held
                .range(share.saturating_add(1)..)
                .flat_map(|(_, edges)| edges)
                .filter_map(|edge| state.edges.get(edge))
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
        self.lock()
            .update((link.edge(), &Weak::new()), |share| share.demands.clear());
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

    /// The lanes waiting for the pool, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn waiting(&self) -> usize {
        self.lock()
            .edges
            .values()
            .map(|share| share.demands.len())
            .sum()
    }

    /// The edges holding pooled credit, for tests.
    #[cfg(all(test, any(feature = "dummy", feature = "native-webrtc")))]
    pub(crate) fn edges(&self) -> usize {
        self.lock()
            .edges
            .values()
            .filter(|share| share.held > 0)
            .count()
    }
}

impl Default for NodeFramePool {
    fn default() -> Self {
        Self::new()
    }
}
