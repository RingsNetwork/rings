#![deny(missing_docs)]
//! Pure topology transition model for Chord.
//!
//! This module is the production home of the algebraic operators previously
//! mirrored only in convergence tests. The mutable [`PeerRing`](crate::dht::PeerRing)
//! shell interprets these pure transitions by writing successor/predecessor
//! fields and by turning [`TopologyAction`](crate::dht::topology::TopologyAction)
//! values into transport actions.
//!
//! State variables:
//! - `R = Z / 2^160`, represented by [`Did`](crate::dht::Did).
//! - `succ[n]` is the bounded successor sequence for node `n`.
//! - `pred[n]` is the optional predecessor for node `n`.
//! - `finger[n][i]` is the optional sparse/no-wrap finger-table entry at slot `i`.
//!
//! Law: join, remove, notify, stabilize, and finger maintenance are pure
//! transitions over this state. Stabilize/notify/finger refinement are monotone
//! over the finite known topology set; their least fixpoint is the converged
//! Chord state plus a finger table derived from that topology.
//!
//! Head law: `head(s)` is the nearest successor other than `local`, the upper
//! end of the local placement interval `(local, head]`.
//! [`step`](crate::dht::topology::step) emits
//! [`TopologyAction::SuccessorHeadChanged`](crate::dht::topology::TopologyAction::SuccessorHeadChanged)`(h)`
//! iff `head(next) = Some(h)` and
//! `head(state) ≠ Some(h)`, once per transition, after the event's own actions.
//! Placement is a function of the ring state, not of the input that changed it,
//! so every input that moves the head (join, admit, remove, update, stabilize)
//! is reported through this one action and the shell schedules the placement
//! repair from it.

use std::collections::BTreeSet;

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
    /// Sparse/no-wrap finger table.
    pub fingers: Vec<Option<Did>>,
    /// Next finger index maintained by the periodic finger fixer.
    pub fix_finger_index: usize,
}

impl TopologyState {
    /// Construct a pure topology state.
    pub fn new(
        local: Did,
        successors: Vec<Did>,
        predecessor: Option<Did>,
        fingers: Vec<Option<Did>>,
        fix_finger_index: usize,
    ) -> Self {
        Self {
            local,
            successors,
            predecessor,
            fingers,
            fix_finger_index,
        }
    }

    /// Every occupied successor, predecessor, and finger slot other than `local`.
    ///
    /// This is the single definition of `Referenced(n, p)`; the predicate and
    /// the set below are both projections of it.
    fn referenced_slots(&self) -> impl Iterator<Item = Did> + '_ {
        self.successors
            .iter()
            .copied()
            .chain(self.predecessor)
            .chain(self.fingers.iter().flatten().copied())
            .filter(move |peer| *peer != self.local)
    }

    /// `Referenced(n, p)`: `p` occupies a successor, predecessor, or finger
    /// slot of `n`, so `n`'s routing state depends on reaching `p`.
    pub fn references(&self, peer: Did) -> bool {
        self.referenced_slots().any(|slot| slot == peer)
    }

    /// `{ p | Referenced(n, p) }`: every peer the local routing state depends on.
    pub fn referenced_peers(&self) -> BTreeSet<Did> {
        self.referenced_slots().collect()
    }
}

/// Pure result of looking up the owner of a DID in local topology state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindSuccessorStep {
    /// The local state can answer with this successor.
    Local(Did),
    /// The query must be forwarded to `next`.
    Remote {
        /// Next hop.
        next: Did,
        /// DID whose successor is being searched.
        did: Did,
    },
}

/// Pure topology input event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyEvent {
    /// A connected peer is introduced to the topology state.
    Join {
        /// Peer learned by the local node.
        peer: Did,
    },
    /// Atomically admit a transport-validated peer and its pending finger continuations.
    Admit {
        /// Peer whose data channel is open.
        peer: Did,
        /// Finger slots whose lookup completed while the peer was handshaking.
        fixed_fingers: Vec<ConditionalFingerUpdate>,
    },
    /// A peer is removed from successor, predecessor, and finger state.
    Remove {
        /// Peer that left or failed.
        peer: Did,
        /// Successor-list transition justified by the caller's evidence.
        successor: SuccessorRemoval,
    },
    /// A successor candidate was accepted by the liveness/interpreter boundary.
    UpdateSuccessor {
        /// Candidate successor.
        successor: Did,
    },
    /// HMCC/Zave notify input: one candidate predecessor notified this node.
    Notify {
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
    /// Periodic finger-fix transition.
    FixFinger,
    /// Apply a reported successor to a fixed finger slot.
    ApplyFinger {
        /// Finger slot to update.
        index: usize,
        /// Successor reported for that slot.
        successor: Did,
    },
}

/// A deferred finger update that may commit only if its source slot has not
/// changed since the lookup result was queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConditionalFingerUpdate {
    /// Finger slot returned by the lookup.
    pub index: usize,
    /// Slot value observed when the update was deferred.
    pub expected: Option<Did>,
}

/// Successor-list evidence attached to a peer-removal transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SuccessorRemoval {
    /// Preserve surviving successor-list entries for an ordinary leave.
    Preserve,
    /// Replace an unavailable head with exactly these transport-validated peers.
    ///
    /// An empty list clears every successor claim. The transition normalizes
    /// ordering, uniqueness, self references, and capacity.
    ReplaceWith(Vec<Did>),
}

/// Pure topology side effect emitted by a transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TopologyAction {
    /// Ask `next` to find `did` and report with the connect handler.
    FindSuccessorForConnect {
        /// Next hop.
        next: Did,
        /// DID being searched.
        did: Did,
    },
    /// Ask `next` to find `did` and report with the finger-fix handler.
    FindSuccessorForFix {
        /// Next hop.
        next: Did,
        /// DID being searched.
        did: Did,
        /// Finger slot to update when the report returns.
        index: usize,
    },
    /// Query this improved successor for its successor list.
    QuerySuccessorList(Did),
    /// Notify this successor that `local` is its predecessor candidate.
    Notify(Did),
    /// The successor head moved to this node (see the head law).
    SuccessorHeadChanged(Did),
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

fn sorted_successors(mut candidates: Vec<Did>, local: Did, capacity: usize) -> Vec<Did> {
    candidates.retain(|&did| did != local);
    candidates.sort_by_key(|&did| dist(local, did));
    candidates.dedup();
    candidates.truncate(capacity);
    candidates
}

/// `Successors(n)`: the nearest forward nodes, ordered by clockwise distance.
pub fn successors(all: &[Did], n: Did, capacity: usize) -> Vec<Did> {
    sorted_successors(all.to_vec(), n, capacity)
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

/// Correct successor list after introducing one candidate successor.
pub fn update_successors(local: Did, current: &[Did], candidate: Did, capacity: usize) -> Vec<Did> {
    let mut candidates = current.to_vec();
    push_unique(&mut candidates, candidate);
    sorted_successors(candidates, local, capacity)
}

fn finger_join(local: Did, current: &[Option<Did>], peer: Did) -> Vec<Option<Did>> {
    let bias = dist(local, peer);
    current
        .iter()
        .copied()
        .enumerate()
        .map(|(slot, old)| {
            let pos = BigUint::from(Did::power_of_two(slot));
            if bias < pos || peer == local {
                old
            } else {
                match old {
                    Some(existing) if dist(local, existing) < bias => old,
                    _ => Some(peer),
                }
            }
        })
        .collect()
}

/// Remove `peer` from every finger slot without erasing valid slots between
/// non-contiguous runs. Each removed run inherits its immediate following hint.
pub(crate) fn remove_finger_peer(current: &[Option<Did>], peer: Did) -> Vec<Option<Did>> {
    let mut next = current.to_vec();
    let mut index = 0;
    while index < next.len() {
        if next.get(index).copied().flatten() != Some(peer) {
            index = index.saturating_add(1);
            continue;
        }

        let run_start = index;
        while next.get(index).copied().flatten() == Some(peer) {
            index = index.saturating_add(1);
        }
        let replacement = next.get(index).copied().flatten();
        for slot in next.iter_mut().take(index).skip(run_start) {
            *slot = replacement;
        }
    }
    next
}

fn finger_set(
    local: Did,
    current: &[Option<Did>],
    index: usize,
    successor: Did,
) -> Vec<Option<Did>> {
    let mut next = current.to_vec();
    if successor != local {
        if let Some(slot) = next.get_mut(index) {
            *slot = Some(successor);
        }
    }
    next
}

/// `Precedes(n, p, id)`: `p` lies on the open arc `(n, id)`, so forwarding to
/// `p` makes strict clockwise progress toward `id`.
fn precedes(local: Did, peer: Did, target: &BigUint) -> bool {
    peer != local && dist(local, peer) < *target
}

/// `ClosestPrecedingFinger(n, id)`: the highest finger slot on the open arc
/// `(n, id)`, or `None` when the sparse table holds no such hint.
fn closest_preceding_finger(state: &TopologyState, target: &BigUint) -> Option<Did> {
    state
        .fingers
        .iter()
        .rev()
        .flatten()
        .copied()
        .find(|peer| precedes(state.local, *peer, target))
}

/// `head(s)`: the nearest successor other than `local`, the upper end of the
/// local placement interval `(local, head]`; `None` when the node stands alone.
pub fn successor_head(state: &TopologyState) -> Option<Did> {
    state
        .successors
        .iter()
        .copied()
        .find(|successor| *successor != state.local)
}

/// Pure Chord successor lookup against one topology state.
///
/// `Local(head)` answers when `did` lies in the local successor interval
/// `(n, head]`; a node without successors answers with itself. Otherwise the
/// query is forwarded to the closest preceding finger, falling back to the
/// successor head. The Chord paper needs no such fallback because its
/// `finger[1]` is the successor, so `closest_preceding_node` always finds a
/// hop; the sparse/no-wrap finger table may hold no finger right after a join
/// or after a run was cleared, and the head fallback restores that invariant.
///
/// `TopologyState` has public fields, so a successor or finger entry equal to
/// `local` is representable; such entries are skipped rather than trusted.
///
/// Post: `Remote { next, .. }` satisfies `precedes(n, next, dist(n, did))` for
/// every state, so every remote step is a strict clockwise advance and never a
/// self hop.
pub fn find_successor(state: &TopologyState, did: Did) -> FindSuccessorStep {
    let Some(head) = successor_head(state) else {
        return FindSuccessorStep::Local(state.local);
    };
    let target = dist(state.local, did);
    if target <= dist(state.local, head) {
        return FindSuccessorStep::Local(head);
    }
    let next = closest_preceding_finger(state, &target).unwrap_or(head);
    FindSuccessorStep::Remote { next, did }
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

fn step_join(state: &TopologyState, peer: Did, capacity: usize) -> TopologyStep {
    if peer == state.local {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    TopologyStep {
        state: TopologyState {
            successors: update_successors(state.local, &state.successors, peer, capacity),
            fingers: finger_join(state.local, &state.fingers, peer),
            ..state.clone()
        },
        actions: vec![TopologyAction::FindSuccessorForConnect {
            next: peer,
            did: state.local,
        }],
    }
}

fn step_admit(
    state: &TopologyState,
    peer: Did,
    fixed_fingers: &[ConditionalFingerUpdate],
    capacity: usize,
) -> TopologyStep {
    if peer == state.local {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }

    let successors = update_successors(state.local, &state.successors, peer, capacity);
    let inserted = !state.successors.contains(&peer) && successors.contains(&peer);
    let mut fingers = finger_join(state.local, &state.fingers, peer);
    for update in fixed_fingers {
        if state.fingers.get(update.index).copied().flatten() == update.expected {
            fingers = finger_set(state.local, &fingers, update.index, peer);
        }
    }

    let mut actions = Vec::new();
    if inserted {
        actions.push(TopologyAction::QuerySuccessorList(peer));
    }
    actions.push(TopologyAction::FindSuccessorForConnect {
        next: peer,
        did: state.local,
    });
    TopologyStep {
        state: TopologyState {
            successors,
            fingers,
            ..state.clone()
        },
        actions,
    }
}

fn step_remove(
    state: &TopologyState,
    peer: Did,
    successor: SuccessorRemoval,
    capacity: usize,
) -> TopologyStep {
    let removed_head = state.successors.first().copied() == Some(peer);
    let mut next_successors = state
        .successors
        .iter()
        .copied()
        .filter(|&did| did != peer)
        .collect::<Vec<_>>();
    let fingers = remove_finger_peer(&state.fingers, peer);
    if removed_head {
        match successor {
            SuccessorRemoval::Preserve => {}
            SuccessorRemoval::ReplaceWith(mut validated) => {
                validated.retain(|candidate| *candidate != peer);
                next_successors = sorted_successors(validated, state.local, capacity);
            }
        }
    }
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            predecessor: state.predecessor.filter(|&did| did != peer),
            fingers,
            ..state.clone()
        },
        actions: Vec::new(),
    }
}

fn step_update_successor(state: &TopologyState, successor: Did, capacity: usize) -> TopologyStep {
    let next_successors = update_successors(state.local, &state.successors, successor, capacity);
    let inserted = !state.successors.contains(&successor) && next_successors.contains(&successor);
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            ..state.clone()
        },
        actions: if inserted {
            vec![TopologyAction::QuerySuccessorList(successor)]
        } else {
            Vec::new()
        },
    }
}

fn step_fix_finger(state: &TopologyState) -> TopologyStep {
    if state.fingers.is_empty() {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    let index = (state.fix_finger_index + 1) % state.fingers.len();
    let did = state.local + Did::power_of_two(index);
    match find_successor(state, did) {
        FindSuccessorStep::Local(successor) => TopologyStep {
            state: TopologyState {
                fingers: finger_set(state.local, &state.fingers, index, successor),
                fix_finger_index: index,
                ..state.clone()
            },
            actions: Vec::new(),
        },
        FindSuccessorStep::Remote { next, did } => TopologyStep {
            state: TopologyState {
                fix_finger_index: index,
                ..state.clone()
            },
            actions: vec![TopologyAction::FindSuccessorForFix { next, did, index }],
        },
    }
}

/// Apply one pure topology transition.
///
/// Post: the returned state depends only on `state` and `event`; no locks,
/// storage, clocks, randomness, or transport effects are read here. The head
/// law holds: `SuccessorHeadChanged(h)` is the last action iff the head moved
/// to `h`.
pub fn step(state: &TopologyState, event: TopologyEvent, capacity: usize) -> TopologyStep {
    let mut next = step_event(state, event, capacity);
    if let Some(head) =
        successor_head(&next.state).filter(|head| successor_head(state) != Some(*head))
    {
        next.actions
            .push(TopologyAction::SuccessorHeadChanged(head));
    }
    next
}

/// The event's own transition, before the head law is applied.
fn step_event(state: &TopologyState, event: TopologyEvent, capacity: usize) -> TopologyStep {
    match event {
        TopologyEvent::Join { peer } => step_join(state, peer, capacity),
        TopologyEvent::Admit {
            peer,
            fixed_fingers,
        } => step_admit(state, peer, &fixed_fingers, capacity),
        TopologyEvent::Remove { peer, successor } => step_remove(state, peer, successor, capacity),
        TopologyEvent::UpdateSuccessor { successor } => {
            step_update_successor(state, successor, capacity)
        }
        TopologyEvent::Notify { predecessor } => TopologyStep {
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
        TopologyEvent::FixFinger => step_fix_finger(state),
        TopologyEvent::ApplyFinger { index, successor } => TopologyStep {
            state: TopologyState {
                fingers: finger_set(state.local, &state.fingers, index, successor),
                ..state.clone()
            },
            actions: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests;
