//! The exhaustive search: safety, coverage, and conditional liveness decided
//! on one explicit state graph.
//!
//! The graph is built breadth-first from `Init` by the model's own
//! `actions`/`next_state`, single-threaded and allocation-only, so the same
//! search runs natively and in the browser test job. Breadth-first order
//! makes the first state that violates an `Always` law a minimal
//! counterexample; a trace is recovered by replaying action indices.
//!
//! States are identified by a 128-bit fingerprint (two independent 64-bit
//! hashes). A collision would merge two states and skip the second's
//! subtree; at a million states the probability is below `2^-88`, and the
//! exact state-count assertions of the tests pin the fingerprint function as
//! much as the carrier. The graph is bounded by [`EXPLORATION_BOUND`], so a
//! carrier that grows past its documented size fails deterministically
//! instead of running until a job timeout.
//!
//! # Liveness
//!
//! Let `G` be the reachable graph of the model and `G_q ⊆ G` its subgraph of
//! *protocol* edges (no environment step). A behaviour whose churn has stopped
//! at state `r` is an infinite path of `G_q` from `r`. It is *fair* iff
//!
//! - `WF(a)` for every dial, delivery, and event `a`: continuously enabled
//!   ⇒ eventually taken; and
//! - `SF(a)` for every periodic `a = Stabilize(p)`: enabled infinitely often
//!   ⇒ taken infinitely often. Production enables each peer's maintenance
//!   timer continuously; the search's one-round-at-a-time bound disables it
//!   while another peer's round is in flight, so the weak fairness of the
//!   production timer is strong fairness of the bounded action.
//!
//! Claim: `∀r ∈ G. Premise(r) ⇒ every fair path of G_q from r satisfies
//! ◇□Converged`. A fair path violates `◇□Converged` iff it visits
//! `¬Converged` infinitely often, i.e. iff it settles into a strongly
//! connected set that contains a `¬Converged` state and admits a fair tour.
//!
//! Decision procedure:
//!
//! ```text
//! explore G breadth-first ──▶ per state: laws, protocol edges, Converged, Premise
//!          │
//!          ▼
//! traps(S), S = G:                     \* Streett emptiness, by recursion
//!   for each SCC C of G_q ∩ S with a cycle:
//!     [∃ weak a enabled throughout C, labelling no edge inside C?] ──▶ no fair cycle in C
//!     B = { strong a enabled somewhere in C, labelling no edge inside C }
//!     [B = ∅] ──▶ C is a fair trap
//!     otherwise ──▶ traps(C minus the states that enable some a ∈ B)
//! starving = ⋃ { C fair trap | C ∩ ¬Converged ≠ ∅ } ∪ { s | ¬Converged(s) ∧ enabled(s) = ∅ }
//!          │
//!          ▼
//! [∃r. Premise(r) ∧ r reaches starving in G_q?] ── yes ──▶ violation
//! ```
//!
//! The recursion is exact: a weak label enabled throughout `C` and never
//! taken in it is starved by every cycle of `C`; a strong label in `B` is
//! starved by exactly the cycles that visit a state enabling it; and when
//! neither obstruction exists, the path that tours all of `C` forever is
//! fair. A fair trap made of `Converged` states only is not a violation: its
//! tour satisfies `□Converged` even though some member could leave.
//!
//! `Stable = { s | every protocol path from s stays in Converged }`, the
//! greatest protocol-closed subset of `Converged`, is reported as the
//! inhabited target: every behaviour that enters it has settled.

use std::collections::hash_map::DefaultHasher;
use std::collections::hash_map::Entry;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::marker::PhantomData;
use std::ops::Index;
use std::ops::IndexMut;

use super::laws;
use super::laws::Expectation;
use super::laws::LawName;
use super::overlay::Overlay;
use super::overlay::OverlayAction;
use super::overlay::OverlayState;

/// Upper bound on the explored states: the largest documented configuration
/// with headroom. Exceeding it is a failure of the bounds, not a slow test.
const EXPLORATION_BOUND: usize = 2_000_000;

/// The domain-separation salt of the second fingerprint half.
const SECOND_HALF_SALT: u64 = 0x9e37_79b9_7f4a_7c15;

/// Index of a state in exploration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct StateIndex(usize);

/// Interned protocol action label, the unit of fairness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Label(usize);

/// A key of one of the dense per-state or per-label tables.
trait TableKey: Copy {
    /// The position this key denotes.
    fn position(self) -> usize;
}

impl TableKey for StateIndex {
    fn position(self) -> usize {
        self.0
    }
}

impl TableKey for Label {
    fn position(self) -> usize {
        self.0
    }
}

/// A dense table keyed by one index type, so a state index cannot address a
/// per-label table or vice versa.
struct Table<K, T> {
    /// Entries in key order.
    entries: Vec<T>,
    /// The key type, carried for the type checker only.
    key: PhantomData<K>,
}

impl<K: TableKey, T> Table<K, T> {
    /// An empty table.
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
            key: PhantomData,
        }
    }

    /// A table of `len` copies of `value`.
    fn filled(value: T, len: usize) -> Self
    where T: Clone {
        Self {
            entries: vec![value; len],
            key: PhantomData,
        }
    }

    /// Number of entries.
    fn len(&self) -> usize {
        self.entries.len()
    }

    /// Append `value` and return its key.
    fn push(&mut self, value: T, key: impl FnOnce(usize) -> K) -> K {
        let position = self.entries.len();
        self.entries.push(value);
        key(position)
    }

    /// `(key, entry)` pairs in key order.
    fn iter(&self, key: impl Fn(usize) -> K) -> impl Iterator<Item = (K, &T)> {
        self.entries
            .iter()
            .enumerate()
            .map(move |(position, entry)| (key(position), entry))
    }
}

impl<K: TableKey, T> Index<K> for Table<K, T> {
    type Output = T;

    fn index(&self, key: K) -> &T {
        &self.entries[key.position()]
    }
}

impl<K: TableKey, T> IndexMut<K> for Table<K, T> {
    fn index_mut(&mut self, key: K) -> &mut T {
        &mut self.entries[key.position()]
    }
}

/// A reachable state that violates an `Always` law, with the minimal trace
/// that reaches it.
#[derive(Debug)]
pub(super) struct SafetyViolation {
    /// The violated law.
    pub(super) law: LawName,
    /// Actions from `Init` to the violating state.
    pub(super) trace: Vec<OverlayAction>,
}

/// A premise state from which a fair behaviour never settles, with its
/// replayable trace.
#[derive(Debug)]
pub(super) struct LivenessViolation {
    /// Actions from `Init` to the state where churn stops.
    pub(super) churn_prefix: Vec<OverlayAction>,
    /// Protocol actions from there into the fair trap or the dead state.
    pub(super) quiescent_suffix: Vec<OverlayAction>,
}

/// The liveness analysis of a graph in which every `Always` law held.
#[derive(Debug)]
pub(super) struct LivenessAnalysis {
    /// States satisfying the premise: the quiescent roots the claim covers.
    pub(super) premise_states: usize,
    /// Premise states outside `Stable`, so the claim had work to do.
    pub(super) unstable_premise_states: usize,
    /// `|Stable|`: the target is inhabited.
    pub(super) stable_states: usize,
    /// The first violation found, if any.
    pub(super) violation: Option<LivenessViolation>,
}

/// What one exhaustive search established.
#[derive(Debug)]
pub(super) enum SearchReport {
    /// An `Always` law failed; exploration stopped at its witness.
    Unsafe {
        /// States visited when the violation was found.
        states: usize,
        /// The violation, with a minimal trace.
        violation: SafetyViolation,
    },
    /// Every `Always` law held on the complete graph.
    Safe {
        /// `|G|`.
        states: usize,
        /// Longest breadth-first level.
        max_depth: usize,
        /// `Sometimes` laws no reachable state satisfied.
        uncovered: Vec<LawName>,
        /// The liveness analysis over the same graph.
        liveness: LivenessAnalysis,
    },
}

/// Where a state was first reached from.
#[derive(Clone, Copy)]
struct Origin {
    /// The state it was expanded from.
    parent: StateIndex,
    /// Index of the action in the parent's enabled list.
    position: usize,
}

/// One protocol edge of the graph.
#[derive(Clone, Copy)]
struct ProtocolEdge {
    /// Interned action label.
    label: Label,
    /// Index of the action in the source's enabled list, for replay.
    position: usize,
    /// The state the action leads to.
    target: StateIndex,
}

/// One explored state: where it came from, what it satisfies, and its
/// protocol edges.
struct Vertex {
    /// How the state was first reached; `None` for `Init`.
    origin: Option<Origin>,
    /// `Converged(s)`.
    converged: bool,
    /// `Premise(s)`.
    premise: bool,
    /// Breadth-first level.
    depth: usize,
    /// Protocol edges out of this state.
    protocol: Vec<ProtocolEdge>,
}

impl Vertex {
    /// The labels enabled at this state.
    fn enabled(&self) -> BTreeSet<Label> {
        self.protocol.iter().map(|edge| edge.label).collect()
    }
}

/// The explored graph: its vertices, which labels are strongly fair, and
/// what the laws found.
struct Graph {
    /// States in exploration order.
    vertices: Table<StateIndex, Vertex>,
    /// `strong[a]`: label `a` is periodic, so its fairness is strong.
    strong: Table<Label, bool>,
    /// First `Always` violation: `(state, law)`; exploration stopped there.
    violation: Option<(StateIndex, LawName)>,
    /// `covered[i]`: some state satisfied the `i`th law (meaningful for
    /// `Sometimes` laws).
    covered: Vec<bool>,
}

impl Graph {
    /// Every state index in exploration order.
    fn states(&self) -> impl Iterator<Item = StateIndex> {
        (0..self.vertices.len()).map(StateIndex)
    }

    /// A per-state table of `value`.
    fn per_state<T: Clone>(&self, value: T) -> Table<StateIndex, T> {
        Table::filled(value, self.vertices.len())
    }
}

/// Deterministic 128-bit fingerprint of a state (`DefaultHasher` has fixed
/// keys; the second half is salted for independence).
fn fingerprint(state: &OverlayState) -> u128 {
    let mut first = DefaultHasher::new();
    state.hash(&mut first);
    let mut second = DefaultHasher::new();
    SECOND_HALF_SALT.hash(&mut second);
    state.hash(&mut second);
    u128::from(first.finish()) << 64 | u128::from(second.finish())
}

/// Explore `G` breadth-first from `Init`, evaluating the laws at every
/// state; the exploration stops at the first `Always` violation.
///
/// Pre: the graph has at most [`EXPLORATION_BOUND`] states; a larger one is
/// a bounds regression and stops the search with that diagnosis.
fn explore(overlay: &Overlay, premise: fn(&OverlayState) -> bool) -> Graph {
    let mut graph = Graph {
        vertices: Table::new(),
        strong: Table::new(),
        violation: None,
        covered: vec![false; laws::LAWS.len()],
    };
    let mut indices = HashMap::<u128, StateIndex>::new();
    let mut labels = HashMap::<OverlayAction, Label>::new();
    let mut queue = VecDeque::<(StateIndex, OverlayState)>::new();
    let discover = |graph: &mut Graph,
                    queue: &mut VecDeque<(StateIndex, OverlayState)>,
                    state: OverlayState,
                    origin: Option<Origin>,
                    depth: usize| {
        assert!(
            graph.vertices.len() < EXPLORATION_BOUND,
            "the reachable graph exceeds {EXPLORATION_BOUND} states at depth {depth}"
        );
        let vertex = Vertex {
            origin,
            converged: laws::is_converged(overlay, &state),
            premise: premise(&state),
            depth,
            protocol: Vec::new(),
        };
        let index = graph.vertices.push(vertex, StateIndex);
        for (position, law) in laws::LAWS.iter().enumerate() {
            let holds = (law.holds)(overlay, &state);
            graph.covered[position] |= holds;
            if law.expectation == Expectation::Always && !holds && graph.violation.is_none() {
                graph.violation = Some((index, law.name));
            }
        }
        queue.push_back((index, state));
        index
    };
    let init = overlay.init();
    let init_fingerprint = fingerprint(&init);
    let init_index = discover(&mut graph, &mut queue, init, None, 0);
    indices.insert(init_fingerprint, init_index);
    'search: while let Some((index, state)) = queue.pop_front() {
        let depth = graph.vertices[index].depth + 1;
        for (position, action) in overlay.actions(&state).into_iter().enumerate() {
            if graph.violation.is_some() {
                break 'search;
            }
            let Some(next) = overlay.next_state(&state, &action) else {
                continue;
            };
            let target = match indices.entry(fingerprint(&next)) {
                Entry::Occupied(known) => *known.get(),
                Entry::Vacant(vacant) => {
                    let origin = Origin {
                        parent: index,
                        position,
                    };
                    *vacant.insert(discover(&mut graph, &mut queue, next, Some(origin), depth))
                }
            };
            if !action.is_environmental() {
                let label = match labels.entry(action) {
                    Entry::Occupied(known) => *known.get(),
                    Entry::Vacant(vacant) => {
                        let periodic = vacant.key().is_periodic();
                        *vacant.insert(graph.strong.push(periodic, Label))
                    }
                };
                graph.vertices[index].protocol.push(ProtocolEdge {
                    label,
                    position,
                    target,
                });
            }
        }
    }
    graph
}

/// The strongly connected components of `G_q` restricted to the states in
/// `within`, by an iterative Tarjan traversal, as member lists.
///
/// `usize::MAX` marks an unvisited state: the arrays are dense and hot, and
/// the sentinel keeps them one word per state.
fn components_within(graph: &Graph, within: &Table<StateIndex, bool>) -> Vec<Vec<StateIndex>> {
    let vertices = &graph.vertices;
    let unvisited = usize::MAX;
    let mut order = graph.per_state(unvisited);
    let mut low = graph.per_state(unvisited);
    let mut on_stack = graph.per_state(false);
    let mut components = Vec::new();
    let mut stack = Vec::new();
    let mut next_order = 0usize;
    for root in graph.states() {
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
                .position(|edge| within[edge.target])
                .map(|offset| cursor + offset);
            match successor {
                Some(edge) => {
                    let target = vertices[vertex].protocol[edge].target;
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

/// The fair traps contained in `within`: the strongly connected sets on
/// which a fair behaviour can remain forever, as member lists. The
/// recursion of the module-level decision procedure.
fn fair_traps(graph: &Graph, within: &Table<StateIndex, bool>) -> Vec<Vec<StateIndex>> {
    let vertices = &graph.vertices;
    let mut traps = Vec::new();
    // A transition always changes the state, so a singleton has no cycle.
    let cyclic = components_within(graph, within)
        .into_iter()
        .filter(|members| members.len() > 1);
    for members in cyclic {
        let mut inside = graph.per_state(false);
        for member in members.iter() {
            inside[*member] = true;
        }
        let taken = members
            .iter()
            .flat_map(|member| vertices[*member].protocol.iter())
            .filter(|edge| inside[edge.target])
            .map(|edge| edge.label)
            .collect::<BTreeSet<_>>();
        let enabled = members
            .iter()
            .map(|member| vertices[*member].enabled())
            .collect::<Vec<_>>();
        let enabled_throughout = enabled.iter().skip(1).fold(
            enabled.first().cloned().unwrap_or_default(),
            |common, next| common.intersection(next).copied().collect(),
        );
        let starves_weak = enabled_throughout
            .iter()
            .any(|label| !graph.strong[*label] && !taken.contains(label));
        if starves_weak {
            continue;
        }
        let starved_strong = enabled
            .iter()
            .flatten()
            .copied()
            .filter(|label| graph.strong[*label] && !taken.contains(label))
            .collect::<BTreeSet<_>>();
        if starved_strong.is_empty() {
            traps.push(members);
            continue;
        }
        for (member, labels) in members.iter().zip(enabled.iter()) {
            inside[*member] = labels.is_disjoint(&starved_strong);
        }
        traps.extend(fair_traps(graph, &inside));
    }
    traps
}

/// The states in which a fair behaviour can remain forever without settling
/// in `Converged`: the members of fair traps that contain an unconverged
/// state, and the dead unconverged states.
fn starvation_states(graph: &Graph) -> Vec<StateIndex> {
    let unconverged = |index: &StateIndex| !graph.vertices[*index].converged;
    let dead = graph
        .vertices
        .iter(StateIndex)
        .filter(|(_, vertex)| !vertex.converged && vertex.protocol.is_empty())
        .map(|(index, _)| index);
    fair_traps(graph, &graph.per_state(true))
        .into_iter()
        .filter(|trap| trap.iter().any(unconverged))
        .flatten()
        .chain(dead)
        .collect()
}

/// `Stable`: the converged states from which no protocol path leaves
/// `Converged`.
fn stable_states(graph: &Graph) -> Table<StateIndex, bool> {
    let unconverged = graph
        .states()
        .filter(|index| !graph.vertices[*index].converged)
        .collect::<Vec<_>>();
    let mut stable = protocol_ancestors(graph, &unconverged);
    for index in graph.states() {
        stable[index] = !stable[index];
    }
    stable
}

/// The action indices leading from `Init` to `target`, by parent pointers.
fn path_from_init(graph: &Graph, target: StateIndex) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut cursor = target;
    while let Some(origin) = graph.vertices[cursor].origin {
        positions.push(origin.position);
        cursor = origin.parent;
    }
    positions.reverse();
    positions
}

/// Replay action indices from `from`, returning the actions and the state
/// reached. This is the deterministic replay of a recorded trace: every
/// index must name an enabled, state-changing action.
fn replay_positions(
    overlay: &Overlay,
    from: OverlayState,
    positions: &[usize],
) -> (Vec<OverlayAction>, OverlayState) {
    let mut state = from;
    let mut trace = Vec::new();
    for position in positions {
        let action = overlay
            .actions(&state)
            .into_iter()
            .nth(*position)
            .unwrap_or_else(|| panic!("recorded action index {position} is not enabled"));
        let next = overlay
            .next_state(&state, &action)
            .unwrap_or_else(|| panic!("recorded action changes nothing: {action:?}"));
        trace.push(action);
        state = next;
    }
    (trace, state)
}

/// A shortest protocol path from `root` into `targets`, as action indices.
fn protocol_path(graph: &Graph, root: StateIndex, targets: &BTreeSet<StateIndex>) -> Vec<usize> {
    let mut origin = HashMap::<StateIndex, Origin>::new();
    let mut queue = VecDeque::from([root]);
    let mut reached = None;
    while let Some(vertex) = queue.pop_front() {
        if targets.contains(&vertex) {
            reached = Some(vertex);
            break;
        }
        for edge in graph.vertices[vertex].protocol.iter() {
            if edge.target != root && !origin.contains_key(&edge.target) {
                origin.insert(edge.target, Origin {
                    parent: vertex,
                    position: edge.position,
                });
                queue.push_back(edge.target);
            }
        }
    }
    let mut positions = Vec::new();
    let mut cursor = reached;
    while let Some(step) = cursor.and_then(|vertex| origin.get(&vertex)) {
        positions.push(step.position);
        cursor = Some(step.parent);
    }
    positions.reverse();
    positions
}

/// The states from which `targets` is reachable over protocol edges.
fn protocol_ancestors(graph: &Graph, targets: &[StateIndex]) -> Table<StateIndex, bool> {
    let mut reversed = graph.per_state(Vec::new());
    for (index, vertex) in graph.vertices.iter(StateIndex) {
        for edge in vertex.protocol.iter() {
            reversed[edge.target].push(index);
        }
    }
    let mut reaches = graph.per_state(false);
    let mut frontier = targets.to_vec();
    while let Some(vertex) = frontier.pop() {
        if std::mem::replace(&mut reaches[vertex], true) {
            continue;
        }
        frontier.extend(reversed[vertex].iter().copied());
    }
    reaches
}

/// Decide the conditional-liveness claim over a graph in which every
/// `Always` law held.
fn analyze_liveness(overlay: &Overlay, graph: &Graph) -> LivenessAnalysis {
    let stable = stable_states(graph);
    let starving = starvation_states(graph);
    let reaches = protocol_ancestors(graph, &starving);
    let violation = graph
        .states()
        .find(|index| graph.vertices[*index].premise && reaches[*index])
        .map(|root| {
            let (churn_prefix, stopped) =
                replay_positions(overlay, overlay.init(), &path_from_init(graph, root));
            let suffix = protocol_path(graph, root, &starving.iter().copied().collect());
            LivenessViolation {
                churn_prefix,
                quiescent_suffix: replay_positions(overlay, stopped, &suffix).0,
            }
        });
    let premise = |index: &StateIndex| graph.vertices[*index].premise;
    LivenessAnalysis {
        premise_states: graph.states().filter(premise).count(),
        unstable_premise_states: graph
            .states()
            .filter(premise)
            .filter(|index| !stable[*index])
            .count(),
        stable_states: graph.states().filter(|index| stable[*index]).count(),
        violation,
    }
}

/// Search `overlay` exhaustively and decide every law, then the
/// conditional-liveness claim under `premise`.
pub(super) fn check(overlay: &Overlay, premise: fn(&OverlayState) -> bool) -> SearchReport {
    let graph = explore(overlay, premise);
    if let Some((index, law)) = graph.violation {
        let trace = replay_positions(overlay, overlay.init(), &path_from_init(&graph, index)).0;
        return SearchReport::Unsafe {
            states: graph.vertices.len(),
            violation: SafetyViolation { law, trace },
        };
    }
    SearchReport::Safe {
        states: graph.vertices.len(),
        max_depth: graph
            .vertices
            .iter(StateIndex)
            .map(|(_, vertex)| vertex.depth)
            .max()
            .unwrap_or(0),
        uncovered: laws::LAWS
            .iter()
            .zip(graph.covered.iter())
            .filter(|(law, covered)| law.expectation == Expectation::Sometimes && !**covered)
            .map(|(law, _)| law.name)
            .collect(),
        liveness: analyze_liveness(overlay, &graph),
    }
}
