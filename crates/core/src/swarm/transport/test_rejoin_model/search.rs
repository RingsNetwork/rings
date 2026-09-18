//! The exhaustive search: safety, coverage, and conditional liveness decided
//! on one explicit state graph.
//!
//! The graph is built breadth-first from `Init` by the model's own
//! `actions`/`next_state`, single-threaded and allocation-only, so the same
//! search runs natively and in the browser test job. Breadth-first order
//! makes the first state that violates an `Always` law a minimal
//! counterexample; a trace is recovered by replaying action indices.
//!
//! # Liveness
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
//! explore G breadth-first ──▶ per state: laws, protocol edges, Converged, Premise
//!          │
//!          ▼
//! Stable = Converged minus the protocol ancestors of ¬Converged
//!          │ a violating behaviour stays in ¬Stable forever
//!          ▼
//! traps(S), S = ¬Stable:              \* Streett emptiness, by recursion
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
//! fair. States are identified by a 64-bit fingerprint, so the graph of a
//! million states stays in memory.

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;

use super::laws;
use super::laws::Expectation;
use super::overlay::Overlay;
use super::overlay::OverlayAction;
use super::overlay::OverlayState;

/// Index of a state in exploration order.
type StateIndex = usize;
/// Interned protocol action label.
type Label = usize;

/// A reachable state that violates an `Always` law, with the minimal trace
/// that reaches it.
#[derive(Debug)]
pub(super) struct SafetyViolation {
    /// The violated law.
    pub(super) law: &'static str,
    /// Actions from `Init` to the violating state.
    pub(super) trace: Vec<OverlayAction>,
}

/// A premise state from which a fair behaviour never stabilizes, with its
/// replayable trace.
#[derive(Debug)]
pub(super) struct LivenessViolation {
    /// Actions from `Init` to the state where churn stops.
    pub(super) churn_prefix: Vec<OverlayAction>,
    /// Protocol actions from there into the fair trap or the dead state.
    pub(super) quiescent_suffix: Vec<OverlayAction>,
}

/// Outcome of one exhaustive search, with the counts that show it was not
/// vacuous.
#[derive(Debug)]
pub(super) struct Verdict {
    /// `|G|`, or the states visited before the safety violation.
    pub(super) states: usize,
    /// Longest breadth-first level explored.
    pub(super) max_depth: usize,
    /// The first `Always` law violated, with a minimal trace.
    pub(super) safety: Option<SafetyViolation>,
    /// `Sometimes` laws no reachable state satisfied.
    pub(super) uncovered: Vec<&'static str>,
    /// States satisfying the premise: the quiescent roots the claim covers.
    pub(super) premise_states: usize,
    /// Premise states outside `Stable`, so the claim had work to do.
    pub(super) unstable_premise_states: usize,
    /// `|Stable|`: the target is inhabited.
    pub(super) stable_states: usize,
    /// The first liveness violation found, if any.
    pub(super) liveness: Option<LivenessViolation>,
}

/// The explored graph: its vertices, which labels are strongly fair, and
/// what the laws found.
struct Graph {
    /// States in exploration order.
    vertices: Vec<Vertex>,
    /// `strong[a]`: label `a` is periodic, so its fairness is strong.
    strong: Vec<bool>,
    /// First `Always` violation: `(state, law)`; exploration stopped there.
    violation: Option<(StateIndex, &'static str)>,
    /// `covered[i]`: some state satisfied the `i`th law (meaningful for
    /// `Sometimes` laws).
    covered: Vec<bool>,
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
    /// Breadth-first level.
    depth: usize,
    /// Protocol edges `(label, action index, target)`.
    protocol: Vec<(Label, usize, StateIndex)>,
}

/// Deterministic fingerprint of a state (`DefaultHasher` has fixed keys).
fn fingerprint(state: &OverlayState) -> u64 {
    let mut hasher = DefaultHasher::new();
    state.hash(&mut hasher);
    hasher.finish()
}

/// Explore `G` breadth-first from `Init`, evaluating the laws at every
/// state; the exploration stops at the first `Always` violation.
fn explore(overlay: &Overlay, premise: fn(&OverlayState) -> bool) -> Graph {
    let laws = laws::laws();
    let mut graph = Graph {
        vertices: Vec::new(),
        strong: Vec::new(),
        violation: None,
        covered: vec![false; laws.len()],
    };
    let mut indices = HashMap::<u64, StateIndex>::new();
    let mut labels = HashMap::<OverlayAction, Label>::new();
    let mut queue = VecDeque::<(StateIndex, OverlayState)>::new();
    let admit = |graph: &mut Graph,
                 queue: &mut VecDeque<(StateIndex, OverlayState)>,
                 state: OverlayState,
                 origin: Option<(StateIndex, usize)>,
                 depth: usize| {
        let index = graph.vertices.len();
        for (position, law) in laws.iter().enumerate() {
            let holds = (law.holds)(overlay, &state);
            graph.covered[position] |= holds;
            if law.expectation == Expectation::Always && !holds && graph.violation.is_none() {
                graph.violation = Some((index, law.name));
            }
        }
        graph.vertices.push(Vertex {
            origin,
            converged: laws::is_converged(overlay, &state),
            premise: premise(&state),
            depth,
            protocol: Vec::new(),
        });
        queue.push_back((index, state));
    };
    let init = overlay.converged_mesh();
    indices.insert(fingerprint(&init), 0);
    admit(&mut graph, &mut queue, init, None, 0);
    while let Some((index, state)) = queue.pop_front() {
        if graph.violation.is_some() {
            break;
        }
        let depth = graph.vertices[index].depth + 1;
        for (position, action) in overlay.actions(&state).into_iter().enumerate() {
            let Some(next) = overlay.next_state(&state, action.clone()) else {
                continue;
            };
            let discovered = graph.vertices.len();
            let target = *indices.entry(fingerprint(&next)).or_insert(discovered);
            if target == discovered {
                admit(&mut graph, &mut queue, next, Some((index, position)), depth);
            }
            if !action.is_environmental() {
                let fresh = labels.len();
                if !labels.contains_key(&action) {
                    graph.strong.push(action.is_periodic());
                }
                let label = *labels.entry(action).or_insert(fresh);
                graph.vertices[index]
                    .protocol
                    .push((label, position, target));
            }
        }
    }
    graph
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
    let Graph {
        vertices, strong, ..
    } = graph;
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
        let Some(action) = overlay.actions(&state).into_iter().nth(*position) else {
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

/// Search `overlay` exhaustively and decide every law, then the
/// conditional-liveness claim under `premise`.
pub(super) fn check(overlay: &Overlay, premise: fn(&OverlayState) -> bool) -> Verdict {
    let graph = explore(overlay, premise);
    let vertices = graph.vertices.as_slice();
    let init = overlay.converged_mesh();
    let max_depth = vertices
        .iter()
        .map(|vertex| vertex.depth)
        .max()
        .unwrap_or(0);
    let uncovered = laws::laws()
        .iter()
        .zip(graph.covered.iter())
        .filter(|(law, covered)| law.expectation == Expectation::Sometimes && !**covered)
        .map(|(law, _)| law.name)
        .collect();
    if let Some((index, law)) = graph.violation {
        let trace = replay_positions(overlay, init, &path_from_init(vertices, index)).0;
        return Verdict {
            states: vertices.len(),
            max_depth,
            safety: Some(SafetyViolation { law, trace }),
            uncovered,
            premise_states: 0,
            unstable_premise_states: 0,
            stable_states: 0,
            liveness: None,
        };
    }
    let stable = stable_states(vertices);
    let starving = starvation_states(&graph, &stable);
    let reaches = protocol_ancestors(vertices, &starving);
    let liveness = (0..vertices.len())
        .find(|index| vertices[*index].premise && reaches[*index])
        .map(|root| {
            let (churn_prefix, stopped) =
                replay_positions(overlay, init, &path_from_init(vertices, root));
            let suffix = protocol_path(vertices, root, &starving.iter().copied().collect());
            LivenessViolation {
                churn_prefix,
                quiescent_suffix: replay_positions(overlay, stopped, &suffix).0,
            }
        });
    let premise = |index: &usize| vertices[*index].premise;
    Verdict {
        states: vertices.len(),
        max_depth,
        safety: None,
        uncovered,
        premise_states: (0..vertices.len()).filter(premise).count(),
        unstable_premise_states: (0..vertices.len())
            .filter(premise)
            .filter(|index| !stable[*index])
            .count(),
        stable_states: stable.iter().filter(|stable| **stable).count(),
        liveness,
    }
}
