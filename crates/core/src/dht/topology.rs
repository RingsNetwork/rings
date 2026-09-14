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

use super::finger::FingerConvergenceState;
use super::finger::FingerConvergenceStatus;
use super::finger::FingerResultDisposition;
#[cfg(test)]
use super::finger::FINGER_LOOKUP_MIN_INTERVAL_MS;
use super::Did;
use super::FingerFixRequest;

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
    finger_convergence: FingerConvergenceState,
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
        let finger_convergence = FingerConvergenceState::new(fingers.len());
        Self {
            local,
            successors,
            predecessor,
            fingers,
            fix_finger_index,
            finger_convergence,
        }
    }

    pub(crate) fn restore(
        local: Did,
        successors: Vec<Did>,
        predecessor: Option<Did>,
        fingers: Vec<Option<Did>>,
        fix_finger_index: usize,
        finger_convergence: FingerConvergenceState,
    ) -> Self {
        let slot_count = fingers.len();
        Self {
            local,
            successors,
            predecessor,
            fingers,
            fix_finger_index,
            finger_convergence: finger_convergence.normalized(slot_count),
        }
    }

    pub(crate) fn finger_convergence_state(&self) -> &FingerConvergenceState {
        &self.finger_convergence
    }

    #[cfg(test)]
    pub(crate) fn finger_convergence_pending(&self) -> bool {
        self.finger_convergence.is_pending()
    }

    pub(crate) fn finger_convergence_status(&self) -> FingerConvergenceStatus {
        if successor_head(self).is_none() {
            FingerConvergenceStatus::inactive()
        } else {
            self.finger_convergence.status()
        }
    }

    #[cfg(all(test, not(target_family = "wasm")))]
    pub(crate) fn finger_convergence_projection(
        &self,
    ) -> super::finger::FingerConvergenceProjection {
        self.finger_convergence.projection()
    }

    pub(crate) fn finger_result_disposition(
        &self,
        request: FingerFixRequest,
        successor: Did,
    ) -> FingerResultDisposition {
        self.finger_convergence.result_disposition(
            self.local,
            self.fingers.len(),
            request,
            successor,
        )
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
        /// Current time used to pace any rejected deferred finger evidence.
        now_ms: u64,
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
    /// Periodic transition that marks one proven finger range for independently paced
    /// revalidation.
    BeginFingerRevalidation,
    /// Independently paced transition that advances pending finger convergence.
    AdvanceFingerConvergence {
        /// Current process-monotonic time used only for lookup rate and expiry bounds.
        now_ms: u64,
        /// Fresh UUID correlation identifier allocated by the effect boundary.
        request_id: uuid::Uuid,
    },
    /// Apply a reported successor to every slot proved by one current lookup.
    ApplyFinger {
        /// Correlation token echoed by the lookup report.
        request: FingerFixRequest,
        /// Successor reported for the request's lowest slot.
        successor: Did,
        /// Current time used to pace a rejected or non-progressing result.
        now_ms: u64,
    },
    /// Cancel an in-flight request after its outbound send failed.
    CancelFinger {
        /// Correlation token of the failed request.
        request: FingerFixRequest,
        /// Current time from which the retry backoff begins.
        now_ms: u64,
    },
}

/// A deferred finger update that may commit only if its source slot has not
/// changed since the lookup result was queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConditionalFingerUpdate {
    /// Correlation token returned while the successor was handshaking.
    pub request: FingerFixRequest,
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
        /// Token the report must echo before it may update the range.
        request: FingerFixRequest,
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

/// Route a verification lookup beyond the local inferred-successor boundary.
///
/// A local `find_successor` answer based only on the current successor head is
/// still a hint: a closer node may exist behind that head. The exact target
/// node is self-proving; every other lookup crosses one admitted transport
/// boundary. A node without a successor does not enter this operation because
/// temporary isolation proves nothing about global membership.
fn finger_verification_route(state: &TopologyState, target: Did) -> FindSuccessorStep {
    match find_successor(state, target) {
        FindSuccessorStep::Local(successor) if successor != state.local && successor != target => {
            FindSuccessorStep::Remote {
                next: successor,
                did: target,
            }
        }
        result => result,
    }
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

/// `Responsible(n, id)`: `id ∈ (pred(n), n]`, so `n` is the Chord successor of the position
/// `id`. Without a known predecessor `n` is responsible only when it stands alone: a node that
/// has successors but has not yet learned its predecessor is uninformed, not responsible for
/// the whole ring.
pub fn is_responsible_for(state: &TopologyState, id: Did) -> bool {
    match state.predecessor {
        Some(predecessor) => {
            id != predecessor && dist(predecessor, id) <= dist(predecessor, state.local)
        }
        None => successor_head(state).is_none(),
    }
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
///
/// Law: `local` is never its own predecessor, so a candidate equal to `local` leaves the
/// current value; the responsibility interval `(pred, local]` is then never empty by a
/// self-reference.
pub fn rectify_predecessor(local: Did, current: Option<Did>, candidate: Did) -> Option<Did> {
    if candidate == local {
        return current;
    }
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
    let fingers = finger_join(state.local, &state.fingers, peer);
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
    TopologyStep {
        state: TopologyState {
            successors: update_successors(state.local, &state.successors, peer, capacity),
            fingers,
            finger_convergence,
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
    now_ms: u64,
    capacity: usize,
) -> TopologyStep {
    if peer == state.local {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }

    let mut verified = state.clone();
    for update in fixed_fingers {
        verified = apply_finger_result(&verified, update.request, peer, now_ms);
    }
    let successors = update_successors(state.local, &state.successors, peer, capacity);
    let inserted = !state.successors.contains(&peer) && successors.contains(&peer);
    let fingers = finger_join(state.local, &verified.fingers, peer);
    let mut finger_convergence = verified.finger_convergence.clone();
    finger_convergence.invalidate_hint_changes(&verified.fingers, &fingers);

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
            finger_convergence,
            ..verified
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
    if removed_head {
        match successor {
            SuccessorRemoval::Preserve => {}
            SuccessorRemoval::ReplaceWith(mut validated) => {
                validated.retain(|candidate| *candidate != peer);
                next_successors = sorted_successors(validated, state.local, capacity);
            }
        }
    }
    let fingers = remove_finger_peer(&state.fingers, peer);
    let mut finger_convergence = state.finger_convergence.clone();
    if next_successors.is_empty() {
        // Losing the last membership witness invalidates even slots whose
        // `None` hint did not change: those empty ranges were proved only in
        // the previous topology.
        finger_convergence.invalidate_all_evidence();
    } else {
        finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
    }
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            predecessor: state.predecessor.filter(|&did| did != peer),
            fingers,
            finger_convergence,
            ..state.clone()
        },
        actions: Vec::new(),
    }
}

fn step_update_successor(state: &TopologyState, successor: Did, capacity: usize) -> TopologyStep {
    let next_successors = update_successors(state.local, &state.successors, successor, capacity);
    let inserted = !state.successors.contains(&successor) && next_successors.contains(&successor);
    let fingers = finger_join(state.local, &state.fingers, successor);
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
    TopologyStep {
        state: TopologyState {
            successors: next_successors,
            fingers,
            finger_convergence,
            ..state.clone()
        },
        actions: if inserted {
            vec![TopologyAction::QuerySuccessorList(successor)]
        } else {
            Vec::new()
        },
    }
}

fn step_fix_finger(state: &TopologyState, now_ms: u64, request_id: uuid::Uuid) -> TopologyStep {
    // A temporarily isolated node has no membership evidence from which it can
    // prove an empty finger range. Keep every slot unverified but dormant; a
    // later successor admission makes the existing state pending again.
    if state.fingers.is_empty() || successor_head(state).is_none() {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    let mut finger_convergence = state.finger_convergence.clone();
    let Some(request) = finger_convergence.prepare_lookup(&state.fingers, now_ms, request_id)
    else {
        return TopologyStep {
            state: TopologyState {
                finger_convergence,
                ..state.clone()
            },
            actions: Vec::new(),
        };
    };
    let index = request.slot_index();
    let did = state.local + Did::power_of_two(index);
    let prepared = TopologyState {
        finger_convergence,
        ..state.clone()
    };
    match finger_verification_route(state, did) {
        FindSuccessorStep::Local(successor) => TopologyStep {
            state: apply_finger_result(&prepared, request, successor, now_ms),
            actions: Vec::new(),
        },
        FindSuccessorStep::Remote { next, did } => TopologyStep {
            state: prepared,
            actions: vec![TopologyAction::FindSuccessorForFix { next, did, request }],
        },
    }
}

fn begin_finger_revalidation(state: &TopologyState) -> TopologyState {
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.begin_revalidation(&state.fingers, state.fix_finger_index);
    TopologyState {
        finger_convergence,
        ..state.clone()
    }
}

fn apply_finger_result(
    state: &TopologyState,
    request: FingerFixRequest,
    successor: Did,
    now_ms: u64,
) -> TopologyState {
    let mut fingers = state.fingers.clone();
    let mut finger_convergence = state.finger_convergence.clone();
    let disposition =
        finger_convergence.apply_result(state.local, &mut fingers, request, successor, now_ms);
    let fix_finger_index = match disposition {
        FingerResultDisposition::Applied { end } => end,
        FingerResultDisposition::Invalid | FingerResultDisposition::Stale => state.fix_finger_index,
    };
    TopologyState {
        fingers,
        fix_finger_index,
        finger_convergence,
        ..state.clone()
    }
}

fn cancel_finger_result(
    state: &TopologyState,
    request: FingerFixRequest,
    now_ms: u64,
) -> TopologyState {
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.cancel(request, now_ms);
    TopologyState {
        finger_convergence,
        ..state.clone()
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
            now_ms,
        } => step_admit(state, peer, &fixed_fingers, now_ms, capacity),
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
            let fingers = next_successors
                .iter()
                .copied()
                .fold(state.fingers.clone(), |fingers, successor| {
                    finger_join(state.local, &fingers, successor)
                });
            let mut finger_convergence = state.finger_convergence.clone();
            finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
            TopologyStep {
                state: TopologyState {
                    successors: next_successors,
                    fingers,
                    finger_convergence,
                    ..state.clone()
                },
                actions,
            }
        }
        TopologyEvent::BeginFingerRevalidation => TopologyStep {
            state: begin_finger_revalidation(state),
            actions: Vec::new(),
        },
        TopologyEvent::AdvanceFingerConvergence { now_ms, request_id } => {
            step_fix_finger(state, now_ms, request_id)
        }
        TopologyEvent::ApplyFinger {
            request,
            successor,
            now_ms,
        } => TopologyStep {
            state: apply_finger_result(state, request, successor, now_ms),
            actions: Vec::new(),
        },
        TopologyEvent::CancelFinger { request, now_ms } => TopologyStep {
            state: cancel_finger_result(state, request, now_ms),
            actions: Vec::new(),
        },
    }
}

#[cfg(test)]
mod convergence_tests;
#[cfg(test)]
mod tests;
