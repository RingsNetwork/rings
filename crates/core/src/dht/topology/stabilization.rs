//! Pure HMCC/Zave predecessor and successor stabilization propositions.
//!
//! # Algorithm flow
//!
//! ```text
//! [BeginStabilize(request_id)]
//!             |
//!             v
//! [current successor head?] -- no --> [clear pending request]
//!             |
//!            yes
//!             v
//! [head's report being processed?] -- yes --> [keep it, emit nothing]
//!             |
//!            no
//!             v
//! [store Requested(reporter, request_id)]
//!             |
//!             v
//! [emit QuerySuccessorTopology]
//!             |
//!             v
//! [authenticated report arrives]
//!             |
//!             v
//! [exact Requested token?] -- no --> [ignore as stale]
//!             |
//!            yes
//!             v
//! [mark Processing and bound candidate list]
//!             |
//!             v
//! [recheck claim before each connection effect]
//!             |
//!             v
//! [merge successors -> query improvement -> notify head]
//!             |
//!             v
//! [reporter stayed head and pred(head) == local?]
//!             |                         |
//!            yes                        no
//!             |                         |
//!             v                         v
//! [prove local finger range]       [retain old proof]
//!             |                         |
//!             +------------+------------+
//!                          |
//!                          v
//!                 [retire correlated token]
//! ```

use super::dist;
use super::push_unique;
use super::rehint;
use super::successors;
use super::ClaimPhase;
use super::Did;
use super::StabilizationRequest;
use super::TopologyAction;
use super::TopologyState;
use super::TopologyStep;
use crate::dht::finger::finger_proof_end;

/// `head(s)`: the nearest successor other than `local`, the upper end of the
/// local placement interval `(local, head]`; `None` when the node stands alone.
pub fn successor_head(state: &TopologyState) -> Option<Did> {
    state
        .successors
        .iter()
        .copied()
        .find(|successor| *successor != state.local)
}

/// The remote peers notified when `state.local` departs voluntarily (#1057): `predecessor`,
/// `successor_head`, and any retained secondary `predecessors`, deduplicated and excluding
/// `state.local`.
///
/// Law: returns at most `capacity + 1` distinct remote peers, empty when the node stands alone,
/// and length 1 on a 2-node ring where `predecessor == successor_head`.
pub fn leave_targets(state: &TopologyState) -> Vec<Did> {
    state
        .predecessor
        .into_iter()
        .chain(successor_head(state))
        .chain(state.predecessors.iter().copied())
        .filter(|&candidate| candidate != state.local)
        .fold(
            Vec::with_capacity(state.predecessors.len().saturating_add(2)),
            |mut targets, candidate| {
                push_unique(&mut targets, candidate);
                targets
            },
        )
}

impl TopologyState {
    /// The remote peers notified when `self.local` departs voluntarily (#1057): see
    /// [`leave_targets`].
    pub fn leave_targets(&self) -> Vec<Did> {
        leave_targets(self)
    }

    /// Whether `self` represents a two-node wrapping ring with `head`: `predecessor` is either
    /// absent or `head`, and no referenced topology slot names a peer other than `head`.
    fn is_two_node_ring_with(&self, head: Did) -> bool {
        self.predecessor.is_none_or(|pred| pred == head)
            && self.referenced_slots().all(|peer| peer == head)
    }

    /// Whether the current successor head `head` permits `candidate` as `self.local`'s
    /// predecessor: `candidate` lies at or beyond `head` clockwise, and equals `head` only on a
    /// two-node wrapping ring.
    fn head_permits_predecessor(&self, head: Did, candidate: Did) -> bool {
        dist(self.local, candidate) >= dist(self.local, head)
            && (candidate != head || self.is_two_node_ring_with(head))
    }
}

/// Highest slot whose target lies in the local successor interval
/// `(local, head]`. That interval is verified by a stabilization report from
/// `head` whose predecessor is `local`, not by routing a query around the ring.
pub(super) fn local_successor_range_end(state: &TopologyState) -> Option<usize> {
    finger_proof_end(state.local, successor_head(state)?, 0, state.fingers.len())
}

/// Correct predecessor value after one HMCC/Zave rectify transition.
///
/// Law: `local` is never its own predecessor, so a candidate equal to `local`
/// leaves the current value; the responsibility interval `(pred, local]` is
/// then never empty by a self-reference.
pub fn rectify_predecessor(local: Did, current: Option<Did>, candidate: Did) -> Option<Did> {
    Some(candidate)
        .filter(|&cand| {
            cand != local && current.is_none_or(|cur| dist(local, cur) < dist(local, cand))
        })
        .or(current)
}

/// Whether `candidate` may be adopted as `state.local`'s predecessor in `state`.
///
/// No peer strictly inside `(local, head)` can be `local`'s predecessor, and
/// `head` itself may only be adopted as predecessor on a two-node wrapping ring
/// where `state` references no peer other than `head` and holds no distinct
/// predecessor.
pub fn can_rectify_predecessor(state: &TopologyState, candidate: Did) -> bool {
    candidate != state.local
        && successor_head(state).is_none_or(|head| state.head_permits_predecessor(head, candidate))
}

/// Correct predecessor value after one topology-aware rectify transition.
pub fn rectify_predecessor_in(state: &TopologyState, candidate: Did) -> Option<Did> {
    let current = state
        .predecessor
        .filter(|&cur| can_rectify_predecessor(state, cur));
    Some(candidate)
        .filter(|&cand| can_rectify_predecessor(state, cand))
        .and_then(|cand| rectify_predecessor(state.local, current, cand))
        .or(current)
}

/// Correct bounded multi-predecessor list after one topology-aware rectify transition.
///
/// Law:
/// 1. Existing entries that no longer satisfy [`can_rectify_predecessor`] (for example, a
///    former 2-node wrapping predecessor `head` after a third peer is admitted) are pruned.
/// 2. If `!can_rectify_predecessor(state, candidate)` or `capacity == 0`, the remaining valid
///    predecessor list is preserved (ordered by descending `dist(local, p)`).
/// 3. If `candidate` is strictly closer to `state.local` counter-clockwise than the current
///    primary predecessor (`dist(local, candidate) > dist(local, primary)`), `candidate`
///    supersedes the list as `vec![candidate]` so that on a transitive ring a newly joined
///    closer predecessor immediately retires stale predecessors behind it.
/// 4. Otherwise (`dist(local, candidate) <= dist(local, primary)`), `candidate` is a sub-ring
///    predecessor behind `primary` that still treats `state.local` as its reachable head; it is
///    inserted into `predecessors` ordered by descending `dist(local, p)`, deduplicated, and
///    truncated to `capacity`.
pub fn rectify_predecessors_in(state: &TopologyState, candidate: Did, capacity: usize) -> Vec<Did> {
    if capacity == 0 {
        return Vec::new();
    }
    let mut current: Vec<Did> = state
        .predecessors
        .iter()
        .copied()
        .chain(state.predecessor)
        .filter(|&peer| can_rectify_predecessor(state, peer))
        .fold(Vec::new(), |mut acc, peer| {
            push_unique(&mut acc, peer);
            acc
        });
    current.sort_by_key(|&peer| std::cmp::Reverse(dist(state.local, peer)));
    current.truncate(capacity);

    if !can_rectify_predecessor(state, candidate) {
        return current;
    }

    match current.first().copied() {
        None => vec![candidate],
        Some(primary) if dist(state.local, candidate) > dist(state.local, primary) => {
            vec![candidate]
        }
        Some(_) => {
            push_unique(&mut current, candidate);
            current.sort_by_key(|&peer| std::cmp::Reverse(dist(state.local, peer)));
            current.truncate(capacity);
            current
        }
    }
}

/// Re-validate and prune `state.predecessor` and `state.predecessors` against the updated
/// topology (`can_rectify_predecessor`), retiring any predecessor that now lies inside
/// `(local, head)` or equals `head` after a two-node ring expands.
pub(super) fn prune_predecessors(state: &mut TopologyState, capacity: usize) {
    let mut valid: Vec<Did> = state
        .predecessors
        .iter()
        .copied()
        .chain(state.predecessor)
        .filter(|&peer| can_rectify_predecessor(state, peer))
        .fold(Vec::new(), |mut acc, peer| {
            push_unique(&mut acc, peer);
            acc
        });
    valid.sort_by_key(|&peer| std::cmp::Reverse(dist(state.local, peer)));
    valid.truncate(capacity);
    state.predecessor = valid.first().copied();
    state.predecessors = valid;
}

/// Correct successor list after one HMCC/Zave stabilize transition.
///
/// The candidate set is the HMCC merge `known = {local} ∪ current ∪ {pred} ∪
/// reported`: unlike Chord's replacement `succ_list ← [s] ++ s.succ_list`, a
/// current entry the reporter has dropped survives until a removal retires
/// it. [`successors`] then removes duplicates, orders candidates by clockwise
/// distance, and enforces `capacity`; it is the only truncation, as Chord's
/// keeping of `r` entries is.
///
/// Law: with `rank(p) = 1 + |{q ∈ known ∖ {local} : d(local, q) < d(local, p)}|`,
/// `∀p ∈ reported. p ∈ result ⇔ rank(p) ≤ capacity`. A reported list carries
/// no terminal self entry (the reporter answers with its successor sequence
/// verbatim), so dropping its last entry would lose a real successor whenever
/// that list is shorter than `capacity` (#786).
pub fn stabilize_successors(
    local: Did,
    current: &[Did],
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> Vec<Did> {
    let known = current
        .iter()
        .copied()
        .chain(topo_predecessor)
        .chain(topo_successors.iter().copied())
        .fold(vec![local], |mut known, candidate| {
            push_unique(&mut known, candidate);
            known
        });
    successors(&known, local, capacity)
}

/// Improved-successor query emitted by one HMCC/Zave stabilize transition.
///
/// A reported predecessor is queried only when it is neither `local` nor at or
/// beyond the current head. This preserves strict clockwise improvement and
/// avoids issuing a redundant query for the existing successor interval.
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
///
/// The nearest normalized successor is notified unless it is the local node.
/// Empty or self-only successor lists therefore produce no network action.
pub fn stabilize_notify(local: Did, next_successors: &[Did]) -> Option<Did> {
    next_successors.first().copied().filter(|&did| did != local)
}

/// Return the local finger range proved by an authenticated current-head report.
///
/// `pred(head) = local` proves that `head = succ(local + 1)`. Requiring the
/// reporter to be both the old and retained head prevents a delayed report from
/// proving a range after the local successor has changed.
pub(super) fn stabilized_successor_proof_end(
    state: &TopologyState,
    reporter: Did,
    next_successors: &[Did],
    reported_predecessor: Option<Did>,
) -> Option<usize> {
    if successor_head(state) != Some(reporter)
        || next_successors.first().copied() != Some(reporter)
        || reported_predecessor != Some(state.local)
    {
        return None;
    }
    finger_proof_end(state.local, reporter, 0, state.fingers.len())
}

/// Start a stabilization query against the current successor head.
///
/// A state with no remote head clears any obsolete request and emits no action.
/// While the head's previous report is still `Processing`, the round is
/// skipped: the handler owning that claim is mid-way through its bounded
/// candidate budget, and superseding it every maintenance period would leave
/// the report unapplied whenever one candidate handshake outlasts the period.
/// Otherwise the exact `(reporter, request_id)` pair is stored in `Requested`
/// phase, superseding an unanswered round, before the matching topology query
/// is emitted.
pub(super) fn step_begin(state: &TopologyState, request_id: uuid::Uuid) -> TopologyStep {
    let Some(reporter) = successor_head(state) else {
        return TopologyStep {
            state: TopologyState {
                pending_stabilization: None,
                ..state.clone()
            },
            actions: Vec::new(),
        };
    };
    if state
        .pending_stabilization
        .is_some_and(|pending| pending.phase == ClaimPhase::Processing)
    {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    TopologyStep {
        state: TopologyState {
            pending_stabilization: Some(StabilizationRequest {
                reporter,
                request_id,
                phase: ClaimPhase::Requested,
            }),
            ..state.clone()
        },
        actions: vec![TopologyAction::QuerySuccessorTopology {
            successor: reporter,
            request_id,
        }],
    }
}

/// Move a matching stabilization report from requested to processing.
///
/// Only the exact current reporter and request token may claim the round. A
/// mismatch leaves the state unchanged, while a match reserves the token for
/// bounded connection effects and later application by [`step_stabilize`].
pub(super) fn step_claim(
    state: &TopologyState,
    reporter: Did,
    request_id: uuid::Uuid,
) -> TopologyStep {
    TopologyStep {
        state: TopologyState {
            pending_stabilization: state
                .can_claim_stabilization_report(reporter, request_id)
                .then_some(StabilizationRequest {
                    reporter,
                    request_id,
                    phase: ClaimPhase::Processing,
                })
                .or(state.pending_stabilization),
            ..state.clone()
        },
        actions: Vec::new(),
    }
}

/// Apply a successor topology report that owns the current claim.
///
/// The report must own the current `Processing` claim or the entire transition
/// is ignored: there is no token-less path by which an unsolicited report can
/// refine successors. A valid report normalizes successor evidence, emits
/// improvement and notification actions, updates finger hints, proves the
/// local finger range when the reporter remains the head and reports `local`
/// as its predecessor, and retires the token.
pub(super) fn step_stabilize(
    state: &TopologyState,
    reporter: Did,
    request_id: uuid::Uuid,
    topo_successors: &[Did],
    topo_predecessor: Option<Did>,
    capacity: usize,
) -> TopologyStep {
    if !state.is_processing_stabilization_report(reporter, request_id) {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    let next_successors = stabilize_successors(
        state.local,
        &state.successors,
        topo_successors,
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
    let mut fingers = next_successors
        .iter()
        .copied()
        .fold(state.fingers.clone(), |fingers, successor| {
            super::finger_join(state.local, &fingers, successor)
        });
    // A head report with `pred(head) == local` proves every finger slot whose
    // target lies inside the local successor range.
    let successor_proof_end =
        stabilized_successor_proof_end(state, reporter, &next_successors, topo_predecessor);
    if let Some(end) = successor_proof_end {
        fingers
            .iter_mut()
            .take(end.saturating_add(1))
            .for_each(|finger| *finger = Some(reporter));
    }
    let (fingers, mut finger_convergence) = rehint(state, fingers);
    if let Some(end) = successor_proof_end {
        finger_convergence.confirm_range(0, end);
    }
    let mut next_state = TopologyState {
        successors: next_successors,
        fingers,
        finger_convergence,
        pending_stabilization: None,
        ..state.clone()
    };
    prune_predecessors(&mut next_state, capacity);
    prune_bridged(&mut next_state, None, capacity);
    TopologyStep {
        state: next_state,
        actions,
    }
}

/// Maximum number of passive bridge hints retained for a single reporter (`2 * capacity`),
/// and across all `capacity` successors (`2 * capacity^2`).
pub(super) const fn bridged_reporter_budget(capacity: usize) -> usize {
    capacity.saturating_mul(2)
}

/// Maximum total passive bridge hints retained in `TopologyState::bridged` (`2 * capacity^2`).
pub(super) const fn bridged_total_capacity(capacity: usize) -> usize {
    capacity.saturating_mul(capacity).saturating_mul(2)
}

/// Prune `state.bridged` so every target is a non-local unreferenced peer, every `via` bridge
/// is a current topology-referenced peer (`state.referenced_peers()`, not equal to
/// `excluded_peer`), and the total map size is bounded by `2 * capacity^2`.
pub(super) fn prune_bridged(
    state: &mut TopologyState,
    excluded_peer: Option<Did>,
    capacity: usize,
) {
    let local = state.local;
    let referenced = state.referenced_peers();
    state.bridged.retain(|&target, &mut via| {
        target != local
            && excluded_peer != Some(target)
            && excluded_peer != Some(via)
            && !referenced.contains(&target)
            && referenced.contains(&via)
    });
    let max_total = bridged_total_capacity(capacity);
    while state.bridged.len() > max_total {
        let Some((&farthest, _)) = state
            .bridged
            .iter()
            .max_by_key(|(&target, _)| dist(local, target))
        else {
            break;
        };
        state.bridged.remove(&farthest);
    }
}

/// Record passive 1-hop bridge reachability hints for unconfirmed peers advertised by an
/// admitted topology reporter (`reporter`), enforcing per-reporter (`2K`) and total (`2K^2`)
/// capacity bounds.
pub(super) fn step_record_bridged_peers(
    state: &TopologyState,
    reporter: Did,
    unconfirmed: &[Did],
    capacity: usize,
) -> TopologyStep {
    if capacity == 0 || reporter == state.local || !state.references(reporter) {
        let mut next_state = state.clone();
        prune_bridged(&mut next_state, None, capacity);
        return TopologyStep {
            state: next_state,
            actions: Vec::new(),
        };
    }
    let reporter_budget = bridged_reporter_budget(capacity);
    let mut bounded_unconfirmed = Vec::with_capacity(reporter_budget);
    for &peer in unconfirmed {
        if bounded_unconfirmed.len() == reporter_budget {
            break;
        }
        if peer != state.local && peer != reporter && !state.references(peer) {
            push_unique(&mut bounded_unconfirmed, peer);
        }
    }
    let mut next_state = state.clone();
    next_state
        .bridged
        .retain(|&target, &mut via| via != reporter || bounded_unconfirmed.contains(&target));
    for peer in bounded_unconfirmed {
        let keep_existing = next_state.bridged.get(&peer).is_some_and(|&existing_via| {
            existing_via != reporter
                && next_state.references(existing_via)
                && dist(state.local, existing_via) < dist(state.local, reporter)
        });
        if !keep_existing {
            next_state.bridged.insert(peer, reporter);
        }
    }
    prune_bridged(&mut next_state, None, capacity);
    TopologyStep {
        state: next_state,
        actions: Vec::new(),
    }
}
