//! The exhaustive search shared by the transport model checks: safety,
//! coverage, and conditional liveness decided on one explicit state graph.
//!
//! A model is an instance of [`CheckedModel`]: an initial state, the enabled
//! actions and the next-state relation, its laws, its convergence predicate,
//! and which actions are environmental (excluded once churn stops) or
//! periodic (strongly fair). The Chord rejoin model (`test_rejoin_model`,
//! #772) and the rerouting model (`test_rerouting_model`, #859) are its
//! instances; the search, the fairness analysis, and the trace replay are one
//! implementation.
//!
//! The graph is built breadth-first from `Init` by the model's own
//! `actions`/`next_state`, single-threaded and allocation-only, so the same
//! search runs natively and in the browser test job. Breadth-first order
//! makes the first state that violates an `Always` law a minimal
//! counterexample; a trace is recovered by replaying action indices.
//!
//! States are identified by a 128-bit fingerprint: two evaluations of the
//! same keyed SipHash, the second over a salted input, treated as
//! approximately independent. A collision would merge two states and skip
//! the second's subtree; at a million states the probability is below
//! `2^-88`. State counts and depths are invariant under any collision-free
//! fingerprint, and the fingerprint itself differs between `wasm32` and
//! 64-bit targets (`usize` hashes at its width), so the same counts on both
//! are what excludes a collision. The graph is bounded by
//! [`EXPLORATION_BOUND`], so a carrier that grows past its documented size
//! fails deterministically instead of running until a job timeout.
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
use std::fmt::Debug;
use std::hash::Hash;
use std::hash::Hasher;
use std::marker::PhantomData;
use std::ops::Index;
use std::ops::IndexMut;

/// What a law claims about the reachable states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Expectation {
    /// `□`: every reachable state satisfies the predicate.
    Always,
    /// `◇`: some reachable state satisfies the predicate (coverage).
    Sometimes,
}

/// One checked proposition over a model's states.
pub(super) struct Law<M: CheckedModel> {
    /// Identity used in verdicts and by the mutation tests.
    pub(super) name: M::LawName,
    /// Whether the predicate must hold everywhere or somewhere.
    pub(super) expectation: Expectation,
    /// The predicate.
    pub(super) holds: fn(&M, &M::State) -> bool,
}

/// A finite transition system the search decides.
///
/// `actions` and `next_state` define `Next`; a `None` successor is a step that
/// changes nothing and is not an edge. `is_converged` is the target of the
/// liveness claim, `is_environmental` separates the adversary's steps (absent
/// from `G_q`), and `is_periodic` marks the strongly fair steps.
pub(super) trait CheckedModel: Sized {
    /// A state of the carrier.
    type State: Hash;
    /// A step, also the fairness unit once interned as a label.
    type Action: Clone + Debug + Eq + Hash;
    /// The identity of a law.
    type LawName: Copy + Debug;

    /// `Init`.
    fn init(&self) -> Self::State;
    /// The actions enabled at `state`, in a deterministic order (replay indexes it).
    fn actions(&self, state: &Self::State) -> Vec<Self::Action>;
    /// The state `action` leads to, or `None` when it changes nothing.
    fn next_state(&self, state: &Self::State, action: &Self::Action) -> Option<Self::State>;
    /// Every checked law, in report order.
    fn laws(&self) -> &[Law<Self>];
    /// `Converged(s)`: the target of the liveness claim.
    fn is_converged(&self, state: &Self::State) -> bool;
    /// Whether the environment, not the protocol, takes `action`.
    fn is_environmental(action: &Self::Action) -> bool;
    /// Whether `action` is strongly fair.
    fn is_periodic(action: &Self::Action) -> bool;
}

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

/// A key of one of the dense per-state or per-label tables: an isomorphism
/// with positions, `at ∘ position = id`.
trait TableKey: Copy {
    /// The position this key denotes.
    fn position(self) -> usize;
    /// The key of a position.
    fn at(position: usize) -> Self;
}

impl TableKey for StateIndex {
    fn position(self) -> usize {
        self.0
    }

    fn at(position: usize) -> Self {
        Self(position)
    }
}

impl TableKey for Label {
    fn position(self) -> usize {
        self.0
    }

    fn at(position: usize) -> Self {
        Self(position)
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
    fn push(&mut self, value: T) -> K {
        let position = self.entries.len();
        self.entries.push(value);
        K::at(position)
    }

    /// Every key in order.
    fn keys(&self) -> impl Iterator<Item = K> {
        (0..self.entries.len()).map(K::at)
    }

    /// `(key, entry)` pairs in key order.
    fn iter(&self) -> impl Iterator<Item = (K, &T)> {
        self.keys().zip(self.entries.iter())
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
pub(super) struct SafetyViolation<LawName, Action> {
    /// The violated law.
    pub(super) law: LawName,
    /// Actions from `Init` to the violating state.
    pub(super) trace: Vec<Action>,
}

/// A premise state from which a fair behaviour never settles, with its
/// replayable trace.
#[derive(Debug)]
pub(super) struct LivenessViolation<Action> {
    /// Actions from `Init` to the state where churn stops.
    pub(super) churn_prefix: Vec<Action>,
    /// Protocol actions from there into the fair trap or the dead state.
    pub(super) quiescent_suffix: Vec<Action>,
}

/// The liveness analysis of a graph in which every `Always` law held.
#[derive(Debug)]
pub(super) struct LivenessAnalysis<Action> {
    /// States satisfying the premise: the quiescent roots the claim covers.
    pub(super) premise_states: usize,
    /// Premise states outside `Stable`, so the claim had work to do.
    pub(super) unstable_premise_states: usize,
    /// `|Stable|`: the target is inhabited.
    pub(super) stable_states: usize,
    /// The first violation found, if any.
    pub(super) violation: Option<LivenessViolation<Action>>,
}

/// What one exhaustive search established.
#[derive(Debug)]
pub(super) enum SearchReport<LawName, Action> {
    /// An `Always` law failed; exploration stopped at its witness.
    Unsafe {
        /// States visited when the violation was found.
        states: usize,
        /// The violation, with a minimal trace.
        violation: SafetyViolation<LawName, Action>,
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
        liveness: LivenessAnalysis<Action>,
    },
}

/// The tree edge by which a state was first reached.
#[derive(Clone, Copy)]
struct ParentEdge {
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
    /// The tree edge by which the state was first reached; `None` for `Init`.
    parent: Option<ParentEdge>,
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
    /// First `Always` violation: `(state, index of the law)`; exploration
    /// stopped there.
    violation: Option<(StateIndex, usize)>,
    /// `covered[i]`: some state satisfied the `i`th law (meaningful for
    /// `Sometimes` laws).
    covered: Vec<bool>,
}

impl Graph {
    /// Every state index in exploration order.
    fn states(&self) -> impl Iterator<Item = StateIndex> {
        self.vertices.keys()
    }

    /// A per-state table of `value`.
    fn per_state<T: Clone>(&self, value: T) -> Table<StateIndex, T> {
        Table::filled(value, self.vertices.len())
    }
}

/// The breadth-first frontier: the graph under construction, the states
/// awaiting expansion, and the fingerprint index that identifies them.
struct Frontier<M: CheckedModel> {
    /// The graph under construction.
    graph: Graph,
    /// States discovered but not yet expanded, with their indices.
    queue: VecDeque<(StateIndex, M::State)>,
    /// Fingerprint → state index.
    indices: HashMap<u128, StateIndex>,
    /// Action → label, the interning of fairness units.
    labels: HashMap<M::Action, Label>,
}

impl<M: CheckedModel> Frontier<M> {
    /// Admit a newly discovered state: evaluate the laws, record it, and
    /// queue it for expansion.
    ///
    /// Pre: the graph has fewer than [`EXPLORATION_BOUND`] states; reaching
    /// the bound is a bounds regression and stops the search with that
    /// diagnosis.
    fn discover(
        &mut self,
        model: &M,
        premise: fn(&M::State) -> bool,
        state: M::State,
        parent: Option<ParentEdge>,
        depth: usize,
    ) -> StateIndex {
        assert!(
            self.graph.vertices.len() < EXPLORATION_BOUND,
            "the reachable graph exceeds {EXPLORATION_BOUND} states at depth {depth}"
        );
        let vertex = Vertex {
            parent,
            converged: model.is_converged(&state),
            premise: premise(&state),
            depth,
            protocol: Vec::new(),
        };
        let index = self.graph.vertices.push(vertex);
        for (position, law) in model.laws().iter().enumerate() {
            let holds = (law.holds)(model, &state);
            self.graph.covered[position] |= holds;
            if law.expectation == Expectation::Always && !holds && self.graph.violation.is_none() {
                self.graph.violation = Some((index, position));
            }
        }
        self.queue.push_back((index, state));
        index
    }

    /// The label of a protocol action, interned on first sight.
    fn label(&mut self, action: M::Action) -> Label {
        match self.labels.entry(action) {
            Entry::Occupied(known) => *known.get(),
            Entry::Vacant(vacant) => {
                let periodic = M::is_periodic(vacant.key());
                *vacant.insert(self.graph.strong.push(periodic))
            }
        }
    }
}

/// Deterministic 128-bit fingerprint of a state (`DefaultHasher` has fixed
/// keys; the second half is salted for independence).
fn fingerprint<S: Hash>(state: &S) -> u128 {
    let mut first = DefaultHasher::new();
    state.hash(&mut first);
    let mut second = DefaultHasher::new();
    SECOND_HALF_SALT.hash(&mut second);
    state.hash(&mut second);
    u128::from(first.finish()) << 64 | u128::from(second.finish())
}

/// Explore `G` breadth-first from `Init`, evaluating the laws at every
/// state; the exploration stops at the first `Always` violation.
fn explore<M: CheckedModel>(model: &M, premise: fn(&M::State) -> bool) -> Graph {
    let mut frontier = Frontier::<M> {
        graph: Graph {
            vertices: Table::new(),
            strong: Table::new(),
            violation: None,
            covered: vec![false; model.laws().len()],
        },
        queue: VecDeque::new(),
        indices: HashMap::new(),
        labels: HashMap::new(),
    };
    let init = model.init();
    let init_fingerprint = fingerprint(&init);
    let init_index = frontier.discover(model, premise, init, None, 0);
    frontier.indices.insert(init_fingerprint, init_index);
    'search: while let Some((index, state)) = frontier.queue.pop_front() {
        let depth = frontier.graph.vertices[index].depth + 1;
        for (position, action) in model.actions(&state).into_iter().enumerate() {
            if frontier.graph.violation.is_some() {
                break 'search;
            }
            let Some(next) = model.next_state(&state, &action) else {
                continue;
            };
            let target = match frontier.indices.entry(fingerprint(&next)) {
                Entry::Occupied(known) => *known.get(),
                Entry::Vacant(vacant) => {
                    // The index is known before discovery: the next push.
                    let discovered = *vacant.insert(StateIndex(frontier.graph.vertices.len()));
                    let parent = ParentEdge {
                        parent: index,
                        position,
                    };
                    let pushed = frontier.discover(model, premise, next, Some(parent), depth);
                    assert_eq!(pushed, discovered, "discovery order is the push order");
                    discovered
                }
            };
            if !M::is_environmental(&action) {
                let label = frontier.label(action);
                frontier.graph.vertices[index].protocol.push(ProtocolEdge {
                    label,
                    position,
                    target,
                });
            }
        }
    }
    frontier.graph
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
        .iter()
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
    while let Some(edge) = graph.vertices[cursor].parent {
        positions.push(edge.position);
        cursor = edge.parent;
    }
    positions.reverse();
    positions
}

/// Replay action indices from `from`, returning the actions and the state
/// reached. This is the deterministic replay of a recorded trace: every
/// index must name an enabled, state-changing action.
fn replay_positions<M: CheckedModel>(
    model: &M,
    from: M::State,
    positions: &[usize],
) -> (Vec<M::Action>, M::State) {
    let mut state = from;
    let mut trace = Vec::new();
    for position in positions {
        let action = model
            .actions(&state)
            .into_iter()
            .nth(*position)
            .unwrap_or_else(|| panic!("recorded action index {position} is not enabled"));
        let next = model
            .next_state(&state, &action)
            .unwrap_or_else(|| panic!("recorded action changes nothing: {action:?}"));
        trace.push(action);
        state = next;
    }
    (trace, state)
}

/// A shortest protocol path from `root` into `targets`, as action indices.
fn protocol_path(graph: &Graph, root: StateIndex, targets: &BTreeSet<StateIndex>) -> Vec<usize> {
    let mut origin = HashMap::<StateIndex, ParentEdge>::new();
    let mut queue = VecDeque::from([root]);
    let mut reached = None;
    while let Some(vertex) = queue.pop_front() {
        if targets.contains(&vertex) {
            reached = Some(vertex);
            break;
        }
        for edge in graph.vertices[vertex].protocol.iter() {
            if edge.target != root && !origin.contains_key(&edge.target) {
                origin.insert(edge.target, ParentEdge {
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
    for (index, vertex) in graph.vertices.iter() {
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
fn analyze_liveness<M: CheckedModel>(model: &M, graph: &Graph) -> LivenessAnalysis<M::Action> {
    let stable = stable_states(graph);
    let starving = starvation_states(graph);
    let reaches = protocol_ancestors(graph, &starving);
    let violation = graph
        .states()
        .find(|index| graph.vertices[*index].premise && reaches[*index])
        .map(|root| {
            let (churn_prefix, stopped) =
                replay_positions(model, model.init(), &path_from_init(graph, root));
            let suffix = protocol_path(graph, root, &starving.iter().copied().collect());
            LivenessViolation {
                churn_prefix,
                quiescent_suffix: replay_positions(model, stopped, &suffix).0,
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

/// Search `model` exhaustively and decide every law, then the
/// conditional-liveness claim under `premise`.
pub(super) fn check<M: CheckedModel>(
    model: &M,
    premise: fn(&M::State) -> bool,
) -> SearchReport<M::LawName, M::Action> {
    let graph = explore(model, premise);
    if let Some((index, position)) = graph.violation {
        let trace = replay_positions(model, model.init(), &path_from_init(&graph, index)).0;
        return SearchReport::Unsafe {
            states: graph.vertices.len(),
            violation: SafetyViolation {
                law: model.laws()[position].name,
                trace,
            },
        };
    }
    SearchReport::Safe {
        states: graph.vertices.len(),
        max_depth: graph
            .vertices
            .iter()
            .map(|(_, vertex)| vertex.depth)
            .max()
            .unwrap_or(0),
        uncovered: model
            .laws()
            .iter()
            .zip(graph.covered.iter())
            .filter(|(law, covered)| law.expectation == Expectation::Sometimes && !**covered)
            .map(|(law, _)| law.name)
            .collect(),
        liveness: analyze_liveness(model, &graph),
    }
}

/// The fair-trap decision on hand-built graphs: each branch of the
/// module-level procedure has a graph that exercises it.
mod tests {
    use super::fair_traps;
    use super::starvation_states;
    use super::Graph;
    use super::Label;
    use super::ProtocolEdge;
    use super::StateIndex;
    use super::Table;
    use super::Vertex;

    /// A graph of `edges` `(source, label, target)` over `states` states,
    /// with `strong` marking the strongly fair labels and `converged` the
    /// converged states.
    fn graph(
        states: usize,
        edges: &[(usize, usize, usize)],
        strong: &[bool],
        converged: &[usize],
    ) -> Graph {
        let mut vertices = Table::new();
        for index in 0..states {
            let protocol = edges
                .iter()
                .filter(|(source, _, _)| *source == index)
                .map(|(_, label, target)| ProtocolEdge {
                    label: Label(*label),
                    position: 0,
                    target: StateIndex(*target),
                })
                .collect();
            vertices.push(Vertex {
                parent: None,
                converged: converged.contains(&index),
                premise: true,
                depth: 0,
                protocol,
            });
        }
        let mut strong_table = Table::new();
        for periodic in strong {
            strong_table.push(*periodic);
        }
        Graph {
            vertices,
            strong: strong_table,
            violation: None,
            covered: Vec::new(),
        }
    }

    /// Law: a plain cycle whose every enabled label is taken inside it is a
    /// fair trap.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_a_cycle_taking_every_enabled_label_is_a_trap() {
        let graph = graph(2, &[(0, 0, 1), (1, 1, 0)], &[false, false], &[]);
        let traps = fair_traps(&graph, &graph.per_state(true));
        assert_eq!(traps.len(), 1);
        assert_eq!(traps[0].len(), 2);
    }

    /// Law: a weak label enabled throughout a cycle and never taken inside
    /// it starves under `WF`, so the cycle is no trap.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_a_weak_label_enabled_throughout_and_untaken_starves_the_cycle() {
        let graph = graph(
            3,
            &[(0, 0, 1), (1, 1, 0), (0, 2, 2), (1, 2, 2)],
            &[false, false, false],
            &[],
        );
        assert!(fair_traps(&graph, &graph.per_state(true)).is_empty());
    }

    /// Law: a strong label enabled at one member and never taken inside the
    /// cycle prunes exactly that member; a residual cycle among the others
    /// is a trap, and a residual path is not.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_a_strong_label_prunes_only_the_states_that_enable_it() {
        // 0 → 1 → 2 → 0 with the strong label 9 leaving from 2 only: pruning
        // 2 leaves the path 0 → 1, which has no cycle.
        let path = graph(
            4,
            &[(0, 0, 1), (1, 1, 2), (2, 2, 0), (2, 9, 3)],
            &[
                false, false, false, false, false, false, false, false, false, true,
            ],
            &[],
        );
        assert!(fair_traps(&path, &path.per_state(true)).is_empty());
        // 0 ⇄ 1 and 1 → 2 → 1 with the strong label 9 leaving from 2 only:
        // pruning 2 leaves the cycle 0 ⇄ 1, a trap.
        let residual = graph(
            4,
            &[(0, 0, 1), (1, 1, 0), (1, 2, 2), (2, 3, 1), (2, 9, 3)],
            &[
                false, false, false, false, false, false, false, false, false, true,
            ],
            &[],
        );
        let traps = fair_traps(&residual, &residual.per_state(true));
        assert_eq!(traps, vec![vec![StateIndex(1), StateIndex(0)]]);
    }

    /// Law: a fair trap made of converged states only is not a starvation,
    /// and a dead unconverged state is.
    #[cfg_attr(target_family = "wasm", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_family = "wasm"), test)]
    fn test_only_traps_with_an_unconverged_member_and_dead_states_starve() {
        let converged_trap = graph(3, &[(0, 0, 1), (1, 1, 0), (1, 2, 2)], &[false; 3], &[0, 1]);
        assert_eq!(starvation_states(&converged_trap), vec![StateIndex(2)]);
        let mixed_trap = graph(2, &[(0, 0, 1), (1, 1, 0)], &[false; 2], &[0]);
        assert_eq!(starvation_states(&mixed_trap).len(), 2);
    }
}
