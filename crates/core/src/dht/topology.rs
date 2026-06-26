#![warn(missing_docs)]
//! Pure topology transition model for Chord.
//!
//! This module is the production home of the algebraic operators previously
//! mirrored only in convergence tests. The mutable [`PeerRing`](super::PeerRing)
//! shell interprets these pure transitions by writing successor/predecessor
//! fields and by turning [`TopologyAction`] values into transport actions.
//!
//! State variables:
//! - `R = Z / 2^160`, represented by [`Did`].
//! - `succ[n]` is the bounded successor sequence for node `n`.
//! - `pred[n]` is the optional predecessor for node `n`.
//!
//! Law: stabilize and rectify are monotone refinements over the finite known
//! topology set. Their least fixpoint is the converged Chord state.

use num_bigint::BigUint;

use super::Did;

/// Ring bit-width; `Did` is `Z/2^160`.
pub const RING_BITS: usize = 160;

/// Default successor-list capacity used by the production builder and tests.
pub const DEFAULT_SUCCESSOR_CAPACITY: usize = 3;

/// Pure per-node topology state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyState {
    /// Local node identifier.
    pub local: Did,
    /// Known successors, ordered by clockwise distance from `local`.
    pub successors: Vec<Did>,
    /// Known predecessor.
    pub predecessor: Option<Did>,
}

impl TopologyState {
    /// Construct a pure topology state.
    pub fn new(local: Did, successors: Vec<Did>, predecessor: Option<Did>) -> Self {
        Self {
            local,
            successors,
            predecessor,
        }
    }
}

/// Pure topology input event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyEvent {
    /// HMCC/Zave rectify input: one candidate predecessor notified this node.
    Rectify {
        /// Candidate predecessor.
        predecessor: Did,
    },
    /// HMCC/Zave stabilize input: topological information returned by the
    /// current successor.
    Stabilize {
        /// Successor list reported by the successor.
        successors: Vec<Did>,
        /// Predecessor reported by the successor.
        predecessor: Option<Did>,
    },
}

/// Pure topology side effect emitted by a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyAction {
    /// Query this improved successor for its successor list.
    QuerySuccessorList(Did),
    /// Notify this successor that `local` is its predecessor candidate.
    Notify(Did),
}

/// Result of applying one pure topology transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologyStep {
    /// Next topology state.
    pub state: TopologyState,
    /// Actions to be interpreted by the effect layer.
    pub actions: Vec<TopologyAction>,
}

/// `dist(a,b) == (b - a) mod 2^160`, the clockwise distance from `a` to `b`.
pub fn dist(a: Did, b: Did) -> BigUint {
    BigUint::from(b - a)
}

fn push_unique(xs: &mut Vec<Did>, x: Did) {
    if !xs.contains(&x) {
        xs.push(x);
    }
}

/// `Successors(n)`: the nearest forward nodes, ordered by clockwise distance.
pub fn successors(all: &[Did], n: Did, capacity: usize) -> Vec<Did> {
    let mut others: Vec<Did> = all.iter().copied().filter(|&did| did != n).collect();
    others.sort_by_key(|&did| dist(n, did));
    others.truncate(capacity);
    others
}

/// `Predecessor(n)`: the nearest node behind `n`.
pub fn predecessor(all: &[Did], n: Did) -> Option<Did> {
    all.iter()
        .copied()
        .filter(|&did| did != n)
        .max_by_key(|&did| dist(n, did))
}

/// `Finger(n, bit)`: nearest forward node at distance `>= 2^bit`, else `None`.
///
/// This mirrors Rings' sparse/no-wrap finger table, not the Chord paper's
/// wrapping finger definition.
pub fn finger(all: &[Did], n: Did, bit: usize) -> Option<Did> {
    let threshold = BigUint::from(1u8) << bit;
    all.iter()
        .copied()
        .filter(|&did| did != n && dist(n, did) >= threshold)
        .min_by_key(|&did| dist(n, did))
}

/// Full sparse/no-wrap finger table predicted by the topology operator.
pub fn finger_table(all: &[Did], n: Did) -> Vec<Option<Did>> {
    (0..RING_BITS).map(|bit| finger(all, n, bit)).collect()
}

/// Correct predecessor value after one HMCC/Zave rectify transition.
pub fn rectify_predecessor(local: Did, current: Option<Did>, candidate: Did) -> Option<Did> {
    match current {
        Some(cur) if dist(local, cur) >= dist(local, candidate) => Some(cur),
        _ => Some(candidate),
    }
}

/// Correct successor list after one HMCC/Zave stabilize transition.
pub fn stabilize_successors(
    local: Did,
    current: &[Did],
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> Vec<Did> {
    let mut known = vec![local];
    for &did in current {
        push_unique(&mut known, did);
    }
    if let Some(pred) = topo_predecessor {
        push_unique(&mut known, pred);
    }
    for &did in topo_successors
        .iter()
        .take(topo_successors.len().saturating_sub(1))
    {
        push_unique(&mut known, did);
    }
    successors(&known, local, capacity)
}

/// Improved-successor query emitted by one HMCC/Zave stabilize transition.
pub fn stabilize_query(local: Did, current: &[Did], topo_predecessor: Option<Did>) -> Option<Did> {
    let pred = topo_predecessor?;
    if pred == local {
        return None;
    }
    let old_head = current.iter().copied().min_by_key(|&did| dist(local, did));
    match old_head {
        Some(head) if dist(local, pred) >= dist(local, head) => None,
        _ => Some(pred),
    }
}

/// Notify action emitted after one HMCC/Zave stabilize transition.
pub fn stabilize_notify(local: Did, next_successors: &[Did]) -> Option<Did> {
    next_successors.first().copied().filter(|&did| did != local)
}

/// Apply one pure topology transition.
///
/// Post: the returned state depends only on `state` and `event`; no locks,
/// storage, clocks, randomness, or transport effects are read here.
pub fn step(state: &TopologyState, event: TopologyEvent, capacity: usize) -> TopologyStep {
    match event {
        TopologyEvent::Rectify { predecessor } => TopologyStep {
            state: TopologyState {
                predecessor: rectify_predecessor(state.local, state.predecessor, predecessor),
                ..state.clone()
            },
            actions: Vec::new(),
        },
        TopologyEvent::Stabilize {
            successors: topo_successors,
            predecessor: topo_predecessor,
        } => {
            let next_successors = stabilize_successors(
                state.local,
                &state.successors,
                &topo_successors,
                topo_predecessor,
                capacity,
            );
            let mut actions = Vec::new();
            if let Some(query) = stabilize_query(state.local, &state.successors, topo_predecessor) {
                actions.push(TopologyAction::QuerySuccessorList(query));
            }
            if let Some(notify) = stabilize_notify(state.local, &next_successors) {
                actions.push(TopologyAction::Notify(notify));
            }
            TopologyStep {
                state: TopologyState {
                    successors: next_successors,
                    ..state.clone()
                },
                actions,
            }
        }
    }
}
