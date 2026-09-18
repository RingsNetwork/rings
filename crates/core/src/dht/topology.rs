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
#[cfg(test)]
use super::finger::FINGER_LOOKUP_MIN_INTERVAL_MS;
use super::Did;
use super::FingerFixRequest;

/// Finger convergence adapters for the pure topology state.
mod finger;
/// Named well-formedness, routing, and fixpoint predicates of [`TopologyState`],
/// for the model checkers and simulators of the test build.
#[cfg(test)]
mod invariants;
/// HMCC/Zave successor and predecessor stabilization transitions.
mod stabilization;
/// Correlation state for successor-list synchronization reports.
mod successor_sync;
use finger::advance as step_fix_finger;
pub(crate) use finger::apply as apply_finger;
use finger::apply_result as apply_finger_result;
use finger::begin_revalidation as begin_finger_revalidation;
use finger::cancel as cancel_finger_result;
pub(crate) use finger::defer as defer_finger;
pub(crate) use finger::retire_candidate as retire_finger_candidate;
use stabilization::local_successor_range_end;
pub use stabilization::rectify_predecessor;
pub use stabilization::stabilize_notify;
pub use stabilization::stabilize_query;
pub use stabilization::stabilize_successors;
use stabilization::step_begin;
use stabilization::step_claim;
use stabilization::step_stabilize;
pub use stabilization::successor_head;
pub(crate) use stabilization::StabilizationConnectionPlan;
pub(crate) use stabilization::StabilizationConnectionStep;
pub(crate) use successor_sync::SuccessorSyncConnectionPlan;
pub(crate) use successor_sync::SuccessorSyncConnectionStep;
pub(crate) use successor_sync::SuccessorSyncState;

/// Ring bit-width; `Did` is `Z/2^160`.
pub const RING_BITS: usize = 160;

/// Default successor-list capacity used by the production builder and tests.
pub const DEFAULT_SUCCESSOR_CAPACITY: usize = 3;

/// Pure per-node topology state.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TopologyState {
    /// Local node identifier.
    pub local: Did,
    /// Known successors, ordered by clockwise distance from `local`.
    pub successors: Vec<Did>,
    /// Known predecessor.
    pub predecessor: Option<Did>,
    /// Sparse/no-wrap finger table.
    pub fingers: Vec<Option<Did>>,
    /// Per-slot proof, lookup, retry, and admission state for finger convergence.
    ///
    /// This metadata is kept beside `fingers` so every pure transition updates
    /// the visible hints and the evidence authorizing them atomically. Its width
    /// is normalized to `fingers.len()` whenever a shell snapshot is restored.
    finger_convergence: FingerConvergenceState,
    /// Exact stabilization report currently allowed to refine this state.
    ///
    /// At most one successor-head report may be `Requested` or `Processing`.
    /// Moving the successor head clears the token, preventing delayed reports
    /// from mutating the new topology or proving its local finger range.
    pending_stabilization: Option<StabilizationRequest>,
}

/// Exact stabilization query whose authenticated response may refine the
/// current successor view.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) struct StabilizationRequest {
    /// Successor head that was queried when this request was created.
    ///
    /// The reporter must still be the current head when the response is claimed
    /// and applied; otherwise topology churn has made the evidence stale.
    reporter: Did,
    /// Fresh correlation token that the authenticated report must echo.
    ///
    /// Equality on this token separates overlapping rounds sent to the same
    /// reporter and prevents replay of an earlier authenticated response.
    request_id: uuid::Uuid,
    /// Whether the report has been reserved by the effect handler.
    ///
    /// `Requested` permits one exact claim; `Processing` permits the bounded
    /// connection plan and final stabilization transition owned by that claim.
    phase: StabilizationPhase,
}

/// Claim phase for a stabilization report token.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum StabilizationPhase {
    /// Query was sent and the first matching report may claim it.
    Requested,
    /// A report claimed the token and may spend its bounded connection budget.
    Processing,
}

impl TopologyState {
    /// Construct a pure topology state.
    pub fn new(
        local: Did,
        successors: Vec<Did>,
        predecessor: Option<Did>,
        fingers: Vec<Option<Did>>,
    ) -> Self {
        let finger_convergence = FingerConvergenceState::new(fingers.len());
        Self {
            local,
            successors,
            predecessor,
            fingers,
            finger_convergence,
            pending_stabilization: None,
        }
    }

    /// Restore a state snapshot from the mutable peer-ring shell.
    ///
    /// The finger convergence state is normalized to the current table width so
    /// a resized table cannot retain out-of-range proof or lookup metadata. All
    /// other values are copied verbatim because the shell already owns their
    /// transport and persistence validation; this constructor only restores the
    /// pure transition model's atomic snapshot.
    pub(crate) fn restore(
        local: Did,
        successors: Vec<Did>,
        predecessor: Option<Did>,
        fingers: Vec<Option<Did>>,
        finger_convergence: FingerConvergenceState,
        pending_stabilization: Option<StabilizationRequest>,
    ) -> Self {
        let slot_count = fingers.len();
        Self {
            local,
            successors,
            predecessor,
            fingers,
            finger_convergence: finger_convergence.normalized(slot_count),
            pending_stabilization,
        }
    }

    /// Return the pending stabilization token for peer-ring persistence.
    ///
    /// The value includes reporter, request identity, and claim phase, allowing
    /// the mutable shell to round-trip the pure model without widening field
    /// visibility or reconstructing correlation state from transport events.
    pub(crate) const fn pending_stabilization(&self) -> Option<StabilizationRequest> {
        self.pending_stabilization
    }

    /// Whether an authenticated report may claim this exact stabilization token.
    ///
    /// The predicate accepts only a byte-for-byte reporter/token match in the
    /// `Requested` phase. Reports for a superseded round and replays of a report
    /// already moved to `Processing` both return `false`.
    pub(crate) fn can_claim_stabilization_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> bool {
        self.pending_stabilization
            == Some(StabilizationRequest {
                reporter,
                request_id,
                phase: StabilizationPhase::Requested,
            })
    }

    /// Whether a claimed report is still allowed to perform connection effects.
    ///
    /// The predicate requires the exact reporter/token pair to remain in the
    /// `Processing` phase. Connection plans re-evaluate it before every effect,
    /// so cancellation or head churn revokes unspent work immediately.
    pub(crate) fn is_processing_stabilization_report(
        &self,
        reporter: Did,
        request_id: uuid::Uuid,
    ) -> bool {
        self.pending_stabilization
            == Some(StabilizationRequest {
                reporter,
                request_id,
                phase: StabilizationPhase::Processing,
            })
    }

    /// Borrow the finger convergence metadata attached to this topology snapshot.
    ///
    /// Callers receive an immutable view so proof ranges, in-flight ownership,
    /// and retry deadlines can be persisted or inspected without changing them
    /// independently of the corresponding topology state.
    pub(crate) fn finger_convergence_state(&self) -> &FingerConvergenceState {
        &self.finger_convergence
    }

    #[cfg(test)]
    /// Report whether any finger slot still lacks current proof in tests.
    ///
    /// This test-only semantic projection avoids exposing the convergence
    /// representation. It is true whenever at least one slot remains pending,
    /// regardless of whether that slot is waiting, in flight, or backing off.
    pub(crate) fn finger_convergence_pending(&self) -> bool {
        self.finger_convergence.is_pending()
    }

    /// Compute scheduling status for finger convergence from this topology snapshot.
    ///
    /// Stabilization-proved local slots are excluded from routed lookup work. A
    /// node with no successor head is reported inactive; otherwise the result
    /// describes whether a remote slot is due now or at a later monotonic time.
    pub(crate) fn finger_convergence_status(&self, now_ms: u64) -> FingerConvergenceStatus {
        match local_successor_range_end(self).map(|end| end.saturating_add(1)) {
            Some(first_routable_slot) => self
                .finger_convergence
                .status_after(first_routable_slot, now_ms),
            None => FingerConvergenceStatus::dormant(),
        }
    }

    #[cfg(test)]
    /// Return a stable test projection of internal finger convergence state.
    ///
    /// The projection exposes proof, lookup, and retry facts needed by model
    /// tests while withholding mutable implementation details. Production code
    /// cannot call this method because it is compiled only for tests.
    pub(crate) fn finger_convergence_projection(
        &self,
    ) -> super::finger::FingerConvergenceProjection {
        self.finger_convergence.projection()
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

/// The bounded connection budget of one topology report.
///
/// Keeps the first `capacity` distinct peers of `candidates`, in the order
/// given, skipping `local`. Every report-driven connection plan is built from
/// this one definition, so an untrusted report of any length can cause at most
/// `capacity` connection effects: the successor capacity for a successor-list
/// sync report, one more for a stabilization report's predecessor.
pub(crate) fn bounded_connection_candidates(
    local: Did,
    capacity: usize,
    candidates: impl IntoIterator<Item = Did>,
) -> Vec<Did> {
    let mut bounded = Vec::with_capacity(capacity);
    for candidate in candidates {
        if bounded.len() == capacity {
            break;
        }
        if candidate != local {
            push_unique(&mut bounded, candidate);
        }
    }
    bounded
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
    /// Atomically admit a transport-validated peer and the finger proof, if
    /// any, that waited on it.
    Admit {
        /// Peer whose data channel is open.
        peer: Did,
        /// Correlation token of the proof deferred while the peer was
        /// handshaking.
        ///
        /// Admission replays this exact request against the deferred
        /// convergence lease. If its slot changed or the lease expired during
        /// the handshake, application is rejected instead of overwriting newer
        /// finger evidence. At most one proof exists because the ring owns at
        /// most one attempt.
        deferred_proof: Option<FingerFixRequest>,
        /// Current process-monotonic time used to pace rejected deferred evidence.
        ///
        /// Admission itself does not read a clock; the effect boundary supplies
        /// this value so stale proofs can schedule deterministic retry state.
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
    /// Start one stabilization query against the current successor head.
    BeginStabilize {
        /// Fresh request identity that the authenticated response must echo.
        ///
        /// The transition binds this token to the current successor head before
        /// emitting the topology query, making overlapping rounds distinguishable.
        request_id: uuid::Uuid,
    },
    /// Claim one matching response before it may cause connection effects.
    ClaimStabilize {
        /// Authenticated DID that produced the topology report.
        ///
        /// It must equal the successor head captured by `BeginStabilize`; message
        /// authentication is established before this pure event is constructed.
        reporter: Did,
        /// Correlation identity echoed by the authenticated report.
        ///
        /// Only an exact token still in `Requested` phase may advance to
        /// `Processing`, so duplicate and delayed reports cannot claim work.
        request_id: uuid::Uuid,
    },
    /// HMCC/Zave stabilize input: topological information returned by the
    /// current successor.
    Stabilize {
        /// Peer whose authenticated topology report drives this transition.
        ///
        /// The transition requires this DID to own the current `Processing`
        /// claim; a report from any other peer, or with any other token, is
        /// ignored.
        reporter: Did,
        /// Correlation identity echoed by the authenticated report.
        request_id: uuid::Uuid,
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
        /// Current process-monotonic time used for lookup pacing and expiry.
        ///
        /// The pure transition compares this supplied value with stored
        /// deadlines; it never reads ambient wall-clock or process time.
        now_ms: u64,
        /// Fresh UUID correlation identifier allocated by the effect boundary.
        ///
        /// When a lookup is reserved, this identifier becomes part of the exact
        /// [`FingerFixRequest`] that every result path must echo.
        request_id: uuid::Uuid,
    },
    /// Apply a reported successor to every slot proved by one current lookup.
    ApplyFinger {
        /// Exact correlation token echoed by the authenticated lookup report.
        ///
        /// It identifies the reserved source slot, lookup round, and proof range
        /// that may be committed; stale tokens are rejected without slot changes.
        request: FingerFixRequest,
        /// Successor reported for the request's lowest slot.
        ///
        /// The convergence algorithm verifies that this candidate proves the
        /// requested contiguous range before replacing any finger hints.
        successor: Did,
        /// Current process-monotonic time for expiry and retry accounting.
        ///
        /// Rejected or non-progressing evidence begins deterministic backoff
        /// from this supplied value rather than reading an ambient clock.
        now_ms: u64,
    },
    /// Retain a timely finger proof while its candidate transport is handshaking.
    DeferFinger {
        /// Correlation token whose report arrived before lookup expiry.
        ///
        /// Ownership moves from this live lookup token to an admission lease;
        /// the exact request is later required to apply or retire the evidence.
        request: FingerFixRequest,
        /// Successor proved by the report and awaiting transport admission.
        ///
        /// No finger hint changes until this candidate becomes routable and the
        /// deferred proof is consumed by an `Admit` transition.
        successor: Did,
        /// Current process-monotonic time used to validate report expiry.
        ///
        /// Evidence arriving after the request deadline is rejected and cannot
        /// gain admission ownership.
        now_ms: u64,
    },
    /// Cancel an in-flight request after its outbound send failed.
    CancelFinger {
        /// Correlation token of the failed outbound lookup request.
        ///
        /// Exact matching prevents an old transport failure from cancelling a
        /// newer lookup that happens to target the same finger slot.
        request: FingerFixRequest,
        /// Process-monotonic time from which retry backoff begins.
        ///
        /// Supplying time as event data keeps cancellation deterministic and the
        /// pure topology transition free from clock side effects.
        now_ms: u64,
    },
    /// Retire a stabilization request whose transport send failed.
    CancelStabilize {
        /// Exact stabilization request identity to retire.
        ///
        /// Cancellation removes the pending request only when this token still
        /// matches it; an older send failure cannot cancel a newer round.
        request_id: uuid::Uuid,
    },
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
        ///
        /// It binds the outbound routing action to the reserved slot and lookup
        /// generation recorded in the next topology state.
        request: FingerFixRequest,
    },
    /// Query this improved successor for its successor list.
    QuerySuccessorList(Did),
    /// Notify this successor that `local` is its predecessor candidate.
    Notify(Did),
    /// Query the current successor's topology with an exact response token.
    QuerySuccessorTopology {
        /// Current successor head selected as the report source.
        ///
        /// The resulting response is valid only while this DID remains the head
        /// captured by the pending stabilization request.
        successor: Did,
        /// Correlation token the authenticated report must echo.
        ///
        /// The claim transition uses it to reject duplicate, delayed, or
        /// overlapping stabilization responses from the same successor.
        request_id: uuid::Uuid,
    },
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

/// Append a candidate DID if this small successor/finger worklist does not contain it.
/// Append `x` unless it is already present: the single dedupe rule behind
/// successor merging and candidate bounding.
pub(super) fn push_unique(xs: &mut Vec<Did>, x: Did) {
    if !xs.contains(&x) {
        xs.push(x);
    }
}

/// Replace the hint vector of `state` and version the evidence it changed.
///
/// Every transition that derives new hints goes through this one function, so
/// the hint-change law (a changed slot is stamped with a fresh epoch and
/// unverified, and an attempt whose lower slot changed is retired) has a
/// single owner.
pub(super) fn rehint(
    state: &TopologyState,
    fingers: Vec<Option<Did>>,
) -> (Vec<Option<Did>>, FingerConvergenceState) {
    let mut finger_convergence = state.finger_convergence.clone();
    finger_convergence.invalidate_hint_changes(&state.fingers, &fingers);
    (fingers, finger_convergence)
}

/// Normalize successor candidates by removing `local`, sorting by clockwise
/// distance, deduplicating, and applying the configured capacity.
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

/// Refine sparse/no-wrap finger hints after learning one peer.
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
/// self hop; `TopologyState::routes_clockwise_toward` (test build) names this
/// proposition.
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

/// Pure transition for introducing a connected or discovered peer.
fn step_join(state: &TopologyState, peer: Did, capacity: usize) -> TopologyStep {
    if peer == state.local {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }
    let (fingers, finger_convergence) =
        rehint(state, finger_join(state.local, &state.fingers, peer));
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

/// Pure transition for atomically admitting a transport-validated peer.
fn step_admit(
    state: &TopologyState,
    peer: Did,
    deferred_proof: Option<FingerFixRequest>,
    now_ms: u64,
    capacity: usize,
) -> TopologyStep {
    if peer == state.local {
        return TopologyStep {
            state: state.clone(),
            actions: Vec::new(),
        };
    }

    // The deferred proof is replayed before `peer` is inserted, so it cannot
    // overwrite a slot that changed while admission was in progress.
    let verified = deferred_proof.map_or_else(
        || state.clone(),
        |request| apply_finger_result(state, request, peer, now_ms).0,
    );
    let successors = update_successors(state.local, &state.successors, peer, capacity);
    // Only a newly retained successor needs a follow-up successor-list query.
    let inserted = !state.successors.contains(&peer) && successors.contains(&peer);
    let (fingers, finger_convergence) =
        rehint(&verified, finger_join(state.local, &verified.fingers, peer));

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

/// Pure transition for removing a peer from successor, predecessor, and finger state.
fn step_remove(
    state: &TopologyState,
    peer: Did,
    successor: SuccessorRemoval,
    capacity: usize,
) -> TopologyStep {
    // Head removals may replace the entire successor list with transport-
    // validated evidence; non-head removals preserve the existing successor tail.
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
    let (fingers, mut finger_convergence) = rehint(state, remove_finger_peer(&state.fingers, peer));
    if next_successors.is_empty() {
        // Losing the last membership witness invalidates even slots whose
        // `None` hint did not change: those empty ranges were proved only in
        // the previous topology.
        finger_convergence.invalidate_all_evidence();
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

/// Pure transition for accepting one successor candidate from the effect layer.
fn step_update_successor(state: &TopologyState, successor: Did, capacity: usize) -> TopologyStep {
    let next_successors = update_successors(state.local, &state.successors, successor, capacity);
    // Query the candidate's successor list only if it survived capacity truncation.
    let inserted = !state.successors.contains(&successor) && next_successors.contains(&successor);
    let (fingers, finger_convergence) =
        rehint(state, finger_join(state.local, &state.fingers, successor));
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

/// Apply one pure topology transition.
///
/// Post: the returned state depends only on `state` and `event`; no locks,
/// storage, clocks, randomness, or transport effects are read here. The head
/// law holds: `SuccessorHeadChanged(h)` is the last action iff the head moved
/// to `h`.
pub fn step(state: &TopologyState, event: TopologyEvent, capacity: usize) -> TopologyStep {
    let mut next = step_event(state, event, capacity);
    let previous_head = successor_head(state);
    let next_head = successor_head(&next.state);
    if previous_head != next_head {
        // A stabilization proof is tied to the successor head that was queried.
        next.state.pending_stabilization = None;
    }
    if let Some(head) = next_head.filter(|head| previous_head != Some(*head)) {
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
            deferred_proof,
            now_ms,
        } => step_admit(state, peer, deferred_proof, now_ms, capacity),
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
        TopologyEvent::BeginStabilize { request_id } => step_begin(state, request_id),
        TopologyEvent::ClaimStabilize {
            reporter,
            request_id,
        } => step_claim(state, reporter, request_id),
        TopologyEvent::Stabilize {
            reporter,
            request_id,
            successors: topo_successors,
            predecessor: topo_predecessor,
        } => step_stabilize(
            state,
            reporter,
            request_id,
            &topo_successors,
            topo_predecessor,
            capacity,
        ),
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
        } => apply_finger(state, request, successor, now_ms).0,
        TopologyEvent::DeferFinger {
            request,
            successor,
            now_ms,
        } => defer_finger(state, request, successor, now_ms).0,
        TopologyEvent::CancelFinger { request, now_ms } => TopologyStep {
            state: cancel_finger_result(state, request, now_ms),
            actions: Vec::new(),
        },
        TopologyEvent::CancelStabilize { request_id } => TopologyStep {
            state: TopologyState {
                pending_stabilization: state
                    .pending_stabilization
                    .filter(|pending| pending.request_id != request_id),
                ..state.clone()
            },
            actions: Vec::new(),
        },
    }
}

/// Convergence properties for topology and finger-table fixpoints.
#[cfg(test)]
mod convergence_tests;
/// Unit tests for individual pure topology transitions.
#[cfg(test)]
mod tests;
