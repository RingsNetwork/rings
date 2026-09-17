//! Conditional liveness of the quiescent suffix, decided on the explicit
//! state graph.
//!
//! Stateright's `eventually` is sound only on acyclic behaviours, and the
//! stabilization protocol is periodic, so liveness is decided here instead.
//!
//! Let `G` be the reachable graph of the model and `G_q ⊆ G` its subgraph of
//! *protocol* edges (no environment step). A behaviour whose churn has stopped
//! at state `r` is an infinite path of `G_q` from `r`. It is *fair* iff
//!
//! - `WF(a)` for every delivery and callback `a`: continuously enabled ⇒
//!   eventually taken; and
//! - `SF(a)` for every periodic `a = Stabilize(p)`: enabled infinitely often
//!   ⇒ taken infinitely often. Production enables each peer's maintenance
//!   timer continuously; the search's one-round-at-a-time bound disables it
//!   while another peer's round is in flight, so the weak fairness of the
//!   production timer is strong fairness of the bounded action.
//!
//! Claim: `∀r ∈ G. Premise(r) ⇒ every fair path of G_q from r satisfies
//! ◇□Converged`. A state can sit at the fixpoint while a dead link's close is
//! still undelivered, so the target is not `Converged` but its stable core
//! `Stable = { s | every protocol path from s stays in Converged }`, the
//! greatest protocol-closed subset of `Converged`; `◇□Converged ⇔ ◇Stable`.
//!
//! Decision procedure:
//!
//! ```text
//! explore G breadth-first ──▶ per state: protocol edges, Converged, Premise
//!          │
//!          ▼
//! [Converged closed under protocol edges?] ── no ──▶ Unstable
//!          │ yes: a violating behaviour stays in ¬Converged forever
//!          ▼
//! traps(S), S = ¬Converged:           \* Streett emptiness, by recursion
//!   for each SCC C of G_q ∩ S with a cycle:
//!     [∃ weak a enabled throughout C, labelling no edge inside C?] ──▶ no fair cycle in C
//!     B = { strong a enabled somewhere in C, labelling no edge inside C }
//!     [B = ∅] ──▶ C is a fair trap
//!     otherwise ──▶ traps(C minus the states that enable some a ∈ B)
//! dead(s) ⇔ ¬Stable(s) ∧ enabled(s) = ∅
//!          │
//!          ▼
//! [∃r. Premise(r) ∧ r reaches a trap or a dead state in G_q?] ── yes ──▶ Starved
//! ```
//!
//! The recursion is exact: a weak label enabled throughout `C` and never
//! taken in it is starved by every cycle of `C`; a strong label in `B` is
//! starved by exactly the cycles that visit a state enabling it; and when
//! neither obstruction exists, the path that tours all of `C` forever is
//! fair. States are identified by a 64-bit fingerprint, as in Stateright, so
//! the graph of a million states stays in memory; a trace is recovered by
//! replaying action indices.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;

use stateright::Model;

use super::laws;
use super::overlay::Overlay;
use super::overlay::OverlayAction;
use super::overlay::OverlayState;

/// Index of a state in exploration order.
type StateIndex = usize;
/// Interned protocol action label.
type Label = usize;

/// A premise state from which a fair behaviour never stabilizes, with its
/// replayable trace.
#[derive(Debug)]
pub(super) struct LivenessViolation {
    /// Actions from `Init` to the state where churn stops.
    pub(super) churn_prefix: Vec<OverlayAction>,
    /// Protocol actions from there into the fair trap or the dead state.
    pub(super) quiescent_suffix: Vec<OverlayAction>,
}

/// Outcome of the analysis, with the counts that show it was not vacuous.
#[derive(Debug)]
pub(super) struct SuffixAnalysis {
    /// `|G|`.
    pub(super) states: usize,
    /// States satisfying the premise: the quiescent roots the claim covers.
    pub(super) premise_states: usize,
    /// Premise states outside `Stable`, so the claim had work to do.
    pub(super) unstable_premise_states: usize,
    /// `|Stable|`: the target is inhabited.
    pub(super) stable_states: usize,
    /// The first violation found, if any.
    pub(super) violation: Option<LivenessViolation>,
}

/// The explored graph: its vertices and which labels are strongly fair.
struct Graph {
    /// States in exploration order.
    vertices: Vec<Vertex>,
    /// `strong[a]`: label `a` is periodic, so its fairness is strong.
    strong: Vec<bool>,
}

/// One explored state: where it came from, what it satisfies, and its
/// protocol edges.
struct Vertex {
    /// `(parent, index of the action in the parent's enabled list)`.
    origin: Option<(StateIndex, usize)>,
    /// `Converged(s)`.
    converged: bool,
    /// `Premise(s)`.
    premise: bool,
    /// Protocol edges `(label, action index, target)`.
    protocol: Vec<(Label, usize, StateIndex)>,
}

/// Deterministic fingerprint of a state (`DefaultHasher` has fixed keys).
fn fingerprint(state: &OverlayState) -> u64 {
    let mut hasher = DefaultHasher::new();
    state.hash(&mut hasher);
    hasher.finish()
}

/// Explore `G` breadth-first from `Init`.
fn explore(overlay: &Overlay, premise: fn(&OverlayState) -> bool) -> Graph {
    let mut indices = HashMap::<u64, StateIndex>::new();
    let mut labels = HashMap::<OverlayAction, Label>::new();
    let mut strong = Vec::<bool>::new();
    let mut vertices = Vec::<Vertex>::new();
    let mut queue = VecDeque::<(StateIndex, OverlayState)>::new();
    for init in overlay.init_states() {
        indices.insert(fingerprint(&init), vertices.len());
        queue.push_back((vertices.len(), init.clone()));
        vertices.push(Vertex {
            origin: None,
            converged: laws::is_converged(overlay, &init),
            premise: premise(&init),
            protocol: Vec::new(),
        });
    }
    while let Some((index, state)) = queue.pop_front() {
        let mut enabled = Vec::new();
        overlay.actions(&state, &mut enabled);
        for (position, action) in enabled.into_iter().enumerate() {
            let Some(next) = overlay.next_state(&state, action.clone()) else {
                continue;
            };
            let discovered = vertices.len();
            let target = *indices.entry(fingerprint(&next)).or_insert(discovered);
            if target == discovered {
                vertices.push(Vertex {
                    origin: Some((index, position)),
                    converged: laws::is_converged(overlay, &next),
                    premise: premise(&next),
                    protocol: Vec::new(),
                });
                queue.push_back((target, next));
            }
            if !action.is_environmental() {
                let fresh = labels.len();
                if !labels.contains_key(&action) {
                    strong.push(action.is_periodic());
                }
                let label = *labels.entry(action).or_insert(fresh);
                if let Some(vertex) = vertices.get_mut(index) {
                    vertex.protocol.push((label, position, target));
                }
            }
        }
    }
    Graph { vertices, strong }
}

/// The strongly connected components of `G_q` restricted to the states in
/// `within`, by an iterative Tarjan traversal, as member lists.
fn components_within(vertices: &[Vertex], within: &[bool]) -> Vec<Vec<StateIndex>> {
    let unvisited = usize::MAX;
    let mut order = vec![unvisited; vertices.len()];
    let mut low = vec![unvisited; vertices.len()];
    let mut components = Vec::new();
    let mut on_stack = vec![false; vertices.len()];
    let mut stack = Vec::new();
    let mut next_order = 0usize;
    for root in 0..vertices.len() {
        if !within[root] || order[root] != unvisited {
            continue;
        }
        let mut work = vec![(root, 0usize)];
        while let Some((vertex, cursor)) = work.pop() {
            if cursor == 0 {
                order[vertex] = next_order;
                low[vertex] = next_order;
                next_order += 1;
                stack.push(vertex);
                on_stack[vertex] = true;
            }
            let successor = vertices[vertex]
                .protocol
                .iter()
                .skip(cursor)
                .map(|(_, _, target)| *target)
                .position(|target| within[target])
                .map(|offset| cursor + offset);
            match successor {
                Some(edge) => {
                    let target = vertices[vertex].protocol[edge].2;
                    work.push((vertex, edge + 1));
                    if order[target] == unvisited {
                        work.push((target, 0));
                    } else if on_stack[target] {
                        low[vertex] = low[vertex].min(order[target]);
                    }
                }
                None => {
                    if low[vertex] == order[vertex] {
                        let mut members = Vec::new();
                        while let Some(member) = stack.pop() {
                            on_stack[member] = false;
                            members.push(member);
                            if member == vertex {
                                break;
                            }
                        }
                        components.push(members);
                    }
                    if let Some((parent, _)) = work.last() {
                        low[*parent] = low[*parent].min(low[vertex]);
                    }
                }
            }
        }
    }
    components
}

/// The states of `within` that lie on a fair cycle contained in `within`:
/// the recursion of the module-level decision procedure.
fn fair_traps(graph: &Graph, within: &[bool]) -> Vec<StateIndex> {
    let Graph { vertices, strong } = graph;
    let mut traps = Vec::new();
    // A transition always changes the state, so a singleton has no cycle.
    let cyclic = components_within(vertices, within)
        .into_iter()
        .filter(|members| members.len() > 1);
    for members in cyclic {
        let mut inside = vec![false; vertices.len()];
        for member in members.iter() {
            inside[*member] = true;
        }
        let taken = members
            .iter()
            .flat_map(|member| vertices[*member].protocol.iter())
            .filter(|(_, _, target)| inside[*target])
            .map(|(label, _, _)| *label)
            .collect::<BTreeSet<_>>();
        if taken.is_empty() {
            continue;
        }
        let enabled = members
            .iter()
            .map(|member| {
                vertices[*member]
                    .protocol
                    .iter()
                    .map(|(label, _, _)| *label)
                    .collect::<BTreeSet<_>>()
            })
            .collect::<Vec<_>>();
        let starves_weak = enabled
            .iter()
            .cloned()
            .reduce(|common, next| common.intersection(&next).copied().collect())
            .unwrap_or_default()
            .into_iter()
            .any(|label| !strong[label] && !taken.contains(&label));
        if starves_weak {
            continue;
        }
        let starved_strong = enabled
            .iter()
            .flatten()
            .copied()
            .filter(|label| strong[*label] && !taken.contains(label))
            .collect::<BTreeSet<_>>();
        if starved_strong.is_empty() {
            traps.extend(members);
            continue;
        }
        for (member, labels) in members.iter().zip(enabled.iter()) {
            inside[*member] = labels.is_disjoint(&starved_strong);
        }
        traps.extend(fair_traps(graph, &inside));
    }
    traps
}

/// `Stable`: the converged states from which no protocol path leaves
/// `Converged`.
fn stable_states(vertices: &[Vertex]) -> Vec<bool> {
    let unconverged = (0..vertices.len())
        .filter(|index| !vertices[*index].converged)
        .collect::<Vec<_>>();
    protocol_ancestors(vertices, &unconverged)
        .into_iter()
        .map(|leaves_converged| !leaves_converged)
        .collect()
}

/// The states in which a fair behaviour can remain forever without
/// stabilizing: the members of fair traps of `¬Stable`, and its dead states.
fn starvation_states(graph: &Graph, stable: &[bool]) -> Vec<StateIndex> {
    let unstable = stable.iter().map(|stable| !stable).collect::<Vec<_>>();
    let dead = graph
        .vertices
        .iter()
        .enumerate()
        .filter(|(index, vertex)| unstable[*index] && vertex.protocol.is_empty())
        .map(|(index, _)| index);
    fair_traps(graph, &unstable)
        .into_iter()
        .chain(dead)
        .collect()
}

/// The action indices leading from `Init` to `target`, by parent pointers.
fn path_from_init(vertices: &[Vertex], target: StateIndex) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut cursor = target;
    while let Some((parent, position)) = vertices[cursor].origin {
        positions.push(position);
        cursor = parent;
    }
    positions.reverse();
    positions
}

/// Replay action indices from `from`, returning the actions and the state
/// reached. This is the deterministic replay of a recorded trace.
fn replay_positions(
    overlay: &Overlay,
    from: OverlayState,
    positions: &[usize],
) -> (Vec<OverlayAction>, OverlayState) {
    let mut state = from;
    let mut trace = Vec::new();
    for position in positions {
        let mut enabled = Vec::new();
        overlay.actions(&state, &mut enabled);
        let Some(action) = enabled.into_iter().nth(*position) else {
            break;
        };
        let Some(next) = overlay.next_state(&state, action.clone()) else {
            break;
        };
        trace.push(action);
        state = next;
    }
    (trace, state)
}

/// A shortest protocol path from `root` into `targets`, as action indices.
fn protocol_path(
    vertices: &[Vertex],
    root: StateIndex,
    targets: &BTreeSet<StateIndex>,
) -> Vec<usize> {
    let mut origin = HashMap::<StateIndex, (StateIndex, usize)>::new();
    let mut queue = VecDeque::from([root]);
    let mut reached = None;
    while let Some(vertex) = queue.pop_front() {
        if targets.contains(&vertex) {
            reached = Some(vertex);
            break;
        }
        for (_, position, target) in vertices[vertex].protocol.iter() {
            if *target != root && !origin.contains_key(target) {
                origin.insert(*target, (vertex, *position));
                queue.push_back(*target);
            }
        }
    }
    let mut positions = Vec::new();
    let mut cursor = reached;
    while let Some((parent, position)) = cursor.and_then(|vertex| origin.get(&vertex)) {
        positions.push(*position);
        cursor = Some(*parent);
    }
    positions.reverse();
    positions
}

/// The states from which `targets` is reachable over protocol edges.
fn protocol_ancestors(vertices: &[Vertex], targets: &[StateIndex]) -> Vec<bool> {
    let mut reversed = vec![Vec::new(); vertices.len()];
    for (index, vertex) in vertices.iter().enumerate() {
        for (_, _, target) in vertex.protocol.iter() {
            reversed[*target].push(index);
        }
    }
    let mut reaches = vec![false; vertices.len()];
    let mut frontier = targets.to_vec();
    while let Some(vertex) = frontier.pop() {
        if std::mem::replace(&mut reaches[vertex], true) {
            continue;
        }
        frontier.extend(reversed[vertex].iter().copied());
    }
    reaches
}

/// Decide the conditional-liveness claim for `overlay` under `premise`.
pub(super) fn analyze_quiescent_suffixes(
    overlay: &Overlay,
    premise: fn(&OverlayState) -> bool,
) -> SuffixAnalysis {
    let graph = explore(overlay, premise);
    let vertices = graph.vertices.as_slice();
    let stable = stable_states(vertices);
    let starving = starvation_states(&graph, &stable);
    let reaches = protocol_ancestors(vertices, &starving);
    let violation = (0..vertices.len())
        .find(|index| vertices[*index].premise && reaches[*index])
        .map(|root| {
            let (churn_prefix, stopped) = replay_positions(
                overlay,
                overlay.converged_mesh(),
                &path_from_init(vertices, root),
            );
            let suffix = protocol_path(vertices, root, &starving.iter().copied().collect());
            LivenessViolation {
                churn_prefix,
                quiescent_suffix: replay_positions(overlay, stopped, &suffix).0,
            }
        });
    let premise = |index: &usize| vertices[*index].premise;
    SuffixAnalysis {
        states: vertices.len(),
        premise_states: (0..vertices.len()).filter(premise).count(),
        unstable_premise_states: (0..vertices.len())
            .filter(premise)
            .filter(|index| !stable[*index])
            .count(),
        stable_states: stable.iter().filter(|stable| **stable).count(),
        violation,
    }
}
