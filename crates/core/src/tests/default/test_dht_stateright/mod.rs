//! Mechanism two — model-checking stabilization with Stateright.
//!
//! `test_dht_convergence` pins the SAFETY fixpoint deterministically; here we
//! exhaustively explore the *interleavings* of the stabilization protocol.
//!
//! Stateright requires `State: Clone + PartialEq + Hash`, which a live `PeerRing`
//! (it holds `Arc<Mutex<…>>` and `Box<dyn storage>`) is not. [`DhtSnapshot`]
//! shows the way out — the only state that matters for convergence is
//! `Did`-valued (DID, successors, predecessor, finger), so it round-trips
//! losslessly to/from a real `PeerRing` (proven below), making real chord ops
//! usable from a hashable model state. The models then use the `spec` operators
//! (proven equal to production in `test_dht_convergence`) directly, which is far
//! cheaper for the checker to expand than a storage-backed `PeerRing` per step.
//!
//! Both stages are deliberately scoped (see each stage's SCOPE note); they test
//! sub-behaviours/abstractions, not a faithful model of the full 6-node regime.
//!
//! Staging:
//!   * Stage 1 — the predecessor-update subprotocol under a full-mesh successor
//!     ORACLE: every predecessor converges to the `spec` fixpoint under every
//!     message interleaving. (Not successor discovery.)
//!   * Stage 2 — a small (N=3 star) discovery ABSTRACTION: safety + reachability
//!     hold, and the model exhibits a counterexample showing no bounded-round
//!     convergence (a peer learned after a node's stabilization budget is never
//!     notified) — illustrating the order-sensitivity mechanism behind the
//!     integration test's residual flakiness, not formally pinning the 6-node
//!     configuration.
//!   * Stage 3 — a finite storage CRDT SEC topology model. It explores a
//!     partition-then-merge shape where replicas exchange only join deltas and
//!     checks that every merged state can close to one least upper bound.
//!   * Stage 4 — hand-off cleanup safety for #614 S2'. It abstracts one
//!     placement key through copy -> ack -> delete and checks that local
//!     deletion is reachable only after the successor state contains the key.
//!   * Stage 5 — finger retry resource safety. A finite checker executes the
//!     production topology transition across progress, invalid response,
//!     cancellation, loss, timeout, topology change, duplicate delivery, and
//!     restart. It checks non-reused request correlation, one logical
//!     emission per transition, the per-process interval, and retry backoff.
//!   * Stage 6 — disconnect/rejoin across connection generations (#772). The
//!     composition the stages above leave out: a peer disconnects, is removed
//!     with successor replacement, rejoins under a newer local generation, and
//!     the retired generation's callbacks and reports arrive afterwards. Its
//!     carrier holds the production `TopologyState` *and* the production
//!     `ConnectionLifecycleRegistry`, which is visible only inside
//!     `swarm::transport`, so the model lives at
//!     `swarm::transport::test_rejoin_model`. Unlike the stages here it does
//!     not use Stateright: its own breadth-first search decides safety,
//!     coverage, and fair-suffix liveness in one pass and is plain
//!     single-threaded Rust, so the browser test job runs the same exhaustive
//!     checks against the wasm build. Its module docs carry the TLA+-style
//!     specification, the fairness assumption, the scope limits, and the
//!     bounds table (peers, successor capacity, state counts, CI wall-clock
//!     limit). The topology laws it checks are the named predicates of
//!     `dht::topology::invariants`, shared with any later churn simulator.
//!   * Stage 7 — relay custody across storage-less runs (`#1182`), single-use
//!     next-holder correlation tokens (`#1181`, `Law (Asked)`), chain
//!     replication, and voluntary departure handoff (`#1057`).

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::hash::Hash;

use num_bigint::BigUint;
use stateright::actor::model_timeout;
use stateright::actor::Actor;
use stateright::actor::ActorModel;
use stateright::actor::ActorModelState;
use stateright::actor::Id;
use stateright::actor::Network;
use stateright::actor::Out;
use stateright::Checker;
use stateright::Expectation;
use stateright::Model;

use super::test_dht_convergence::spec;
use super::test_dht_convergence::K;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::storage::MemStorage;

/// A DID at `num/den` of the way round the ring — deterministic test positions.
fn did_frac(num: u64, den: u64) -> Did {
    Did::from((BigUint::from(1u8) << 160) * BigUint::from(num) / BigUint::from(den))
}

/// The hashable topology state of a node — everything in `PeerRing` that drives
/// routing/convergence, and nothing else (no Entry storage/cache).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DhtSnapshot {
    pub did: Did,
    pub succ: Vec<Did>,
    pub pred: Option<Did>,
    pub finger: Vec<Option<Did>>,
}

impl DhtSnapshot {
    /// Snapshot the topology state out of a live `PeerRing`.
    pub(super) fn capture(dht: &PeerRing) -> Self {
        Self {
            did: dht.did,
            succ: dht.successors().list().unwrap(),
            pred: *dht.lock_predecessor().unwrap(),
            finger: dht.lock_finger().unwrap().list().clone(),
        }
    }

    /// Reconstruct a live `PeerRing` carrying this exact topology state, so the
    /// real `chord.rs` operations can run against it. Storage/cache are fresh
    /// (irrelevant to topology).
    pub(super) fn restore(&self) -> PeerRing {
        let dht = PeerRing::new_with_storage(self.did, K as u8, Box::new(MemStorage::new()));
        for &s in &self.succ {
            dht.successors().update(s).unwrap();
        }
        *dht.lock_predecessor().unwrap() = self.pred;
        {
            let mut finger = dht.lock_finger().unwrap();
            for (i, entry) in self.finger.iter().enumerate() {
                if let Some(d) = entry {
                    finger.set(i, *d);
                }
            }
        }
        dht
    }
}

// ===================================================================
// Stage 1: the `notify` predecessor-update subprotocol, under a FULL-MESH
// successor ORACLE.
//
// SCOPE (important): each node notifies `spec::successors(all)` — the *global*
// successor set, i.e. as if every node already knows its final successors.
// Production instead notifies only the selected head after a stabilization
// report commits; that head can still be stale or incomplete. So this stage does
// NOT test successor discovery or full stabilization liveness; it isolates and
// exhausts the delivery interleavings of the predecessor-update rule once
// successors are known. Successor discovery is stage 2.
//
// Maps to the TLA+ `Notify` action and `handlers/stabilization.rs`:
//   on_timeout : each node tells every (oracle) successor "I'm your predecessor".
//   on Send    : apply the `PeerRing::notify` rule to the predecessor.
// ===================================================================

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum Msg {
    /// `NotifyPredecessorSend`: "I think I'm your predecessor."
    NotifyPred { from: Did },
}

/// The periodic stabilization tick (Chord's `stabilize()` runs on a timer).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum Timer {
    Stabilize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct NodeState {
    pred: Option<Did>,
}

/// One Chord node. Holds the full DID set so it can address peers
/// (`Id(i) <-> all[i]`) and run the real notify rule; `all` is identical across
/// every actor.
#[derive(Clone)]
struct ChordNode {
    all: Vec<Did>,
}

impl ChordNode {
    fn did(&self, id: Id) -> Did {
        self.all[usize::from(id)]
    }

    fn id_of(&self, did: Did) -> Id {
        Id::from(
            self.all
                .iter()
                .position(|&d| d == did)
                .expect("did belongs to the modelled set"),
        )
    }

    /// The topology-aware predecessor update (`PeerRing::notify_within_topology`
    /// / `topology::rectify_predecessor_in`, `#1184`), evaluated on the node's
    /// oracle successor list `spec::successors(&self.all, me)`. Used instead of
    /// a throwaway `PeerRing` (which allocates its storage) so the checker can
    /// expand states cheaply. It is NOT assumed equivalent: the test
    /// `test_apply_notify_matches_peer_ring` checks it against the real
    /// `PeerRing::notify_within_topology` across representative states on
    /// `N = 2, 3, 4` rings, so it remains a faithful regression guard.
    fn apply_notify(&self, me: Did, current: Option<Did>, from: Did) -> Option<Did> {
        let state = crate::dht::topology::TopologyState::new(
            me,
            spec::successors(&self.all, me),
            current,
            Vec::new(),
        );
        crate::dht::topology::rectify_predecessor_in(&state, from)
    }
}

impl Actor for ChordNode {
    type Msg = Msg;
    type State = NodeState;
    type Timer = Timer;
    type Random = ();
    type Storage = ();

    fn on_start(&self, _id: Id, _storage: &Option<()>, o: &mut Out<Self>) -> NodeState {
        // Arm the periodic stabilization timer (Chord runs `stabilize()` on a
        // period). The checker explores firing it in every interleaving.
        o.set_timer(Timer::Stabilize, model_timeout());
        NodeState { pred: None }
    }

    fn on_timeout(&self, id: Id, _state: &mut Cow<NodeState>, _timer: &Timer, o: &mut Out<Self>) {
        // Oracle subprotocol: tell each peer in the ring (honest predecessor
        // notifications plus unilateral/adversarial notifications from `me`,
        // `head`, and non-predecessor peers) "I might be your predecessor",
        // then re-arm. The network is a duplicating *set*
        // (`new_unordered_duplicating`), so a re-sent identical notification
        // neither grows the state nor is lost: it stays available to be
        // (re-)delivered in any order while keeping the state space finite.
        let me = self.did(id);
        for &peer in &self.all {
            o.send(self.id_of(peer), Msg::NotifyPred { from: me });
        }
        o.set_timer(Timer::Stabilize, model_timeout());
    }

    fn on_msg(&self, id: Id, state: &mut Cow<NodeState>, _src: Id, msg: Msg, _o: &mut Out<Self>) {
        let me = self.did(id);
        match msg {
            Msg::NotifyPred { from } => {
                let new_pred = self.apply_notify(me, state.pred, from);
                if state.pred != new_pred {
                    state.to_mut().pred = new_pred;
                }
                // The handler would also report its predecessor back so the
                // sender can connect to it; on a full mesh that target is already
                // a known peer, so the report is a no-op for predecessor
                // convergence and is omitted here. Stage 2 (discovery) reinstates
                // it, where the reported peer must actually be connected to.
            }
        }
    }
}

/// Model configuration: the DID set, so property functions (which must be plain
/// `fn` pointers, not closures) can recompute the expected fixpoint.
#[derive(Clone)]
struct Cfg {
    all: Vec<Did>,
}

/// `Always`: every predecessor is well-formed (a real, distinct peer or unset).
fn prop_pred_wellformed(
    model: &ActorModel<ChordNode, Cfg, ()>,
    st: &ActorModelState<ChordNode>,
) -> bool {
    st.actor_states
        .iter()
        .enumerate()
        .all(|(i, s)| match s.pred {
            None => true,
            Some(p) => p != model.cfg.all[i] && model.cfg.all.contains(&p),
        })
}

/// `Always` (`#1184`): on `N >= 3` rings, `predecessor` is never `me` and never
/// `head`, never strictly inside `(me, head)`, and once set to the true
/// `spec::predecessor` no unilateral `NotifyPred` from any ring member can
/// overwrite it.
fn prop_pred_rejects_head_and_interior_preemption(
    model: &ActorModel<ChordNode, Cfg, ()>,
    st: &ActorModelState<ChordNode>,
) -> bool {
    let all = &model.cfg.all;
    let node = ChordNode { all: all.clone() };
    st.actor_states.iter().enumerate().all(|(i, s)| {
        let me = all[i];
        let succs = spec::successors(all, me);
        let head = succs.first().copied();
        let not_self = s.pred != Some(me);
        let not_head_on_multi_ring = all.len() < 3 || s.pred != head;
        let not_inside_head_interval = match (s.pred, head) {
            (Some(p), Some(h)) => spec::dist(me, p) >= spec::dist(me, h),
            _ => true,
        };
        let true_pred = spec::predecessor(all, me);
        let true_pred_non_preemptible = s.pred != true_pred
            || all
                .iter()
                .all(|&candidate| node.apply_notify(me, s.pred, candidate) == true_pred);
        not_self && not_head_on_multi_ring && not_inside_head_interval && true_pred_non_preemptible
    })
}

/// Every node's predecessor equals the formal `spec::predecessor` fixpoint.
fn prop_all_converged(
    model: &ActorModel<ChordNode, Cfg, ()>,
    st: &ActorModelState<ChordNode>,
) -> bool {
    st.actor_states
        .iter()
        .enumerate()
        .all(|(i, s)| s.pred == spec::predecessor(&model.cfg.all, model.cfg.all[i]))
}

fn notify_model(all: Vec<Did>) -> ActorModel<ChordNode, Cfg, ()> {
    let actors: Vec<ChordNode> = all.iter().map(|_| ChordNode { all: all.clone() }).collect();
    // Set-backed duplicating network: identical re-sent notifications (the
    // periodic stabilize timer) collapse into the set (finite state), and stay
    // available to be (re-)delivered in any order — modelling periodic delivery
    // and reordering faithfully.
    ActorModel::new(Cfg { all }, ())
        .actors(actors)
        .init_network(Network::new_unordered_duplicating([]))
        .property(
            Expectation::Always,
            "predecessor well-formed",
            prop_pred_wellformed,
        )
        .property(
            Expectation::Always,
            "rejects head and interior preemption",
            prop_pred_rejects_head_and_interior_preemption,
        )
        .property(
            Expectation::Sometimes,
            "convergence reachable",
            prop_all_converged,
        )
        .property(
            Expectation::Eventually,
            "convergence inevitable",
            prop_all_converged,
        )
}

// ===================================================================
// Stage 2: successor discovery from a star bootstrap — a small ABSTRACTION.
//
// Unlike stage 1, each node's connected-peer set grows DYNAMICALLY: the hub
// learns a spoke only when that spoke's join lookup arrives, so the connect-time
// `find_successor(self)` race is modelled, and the join lookups + stabilize
// query/report chain must drive discovery.
//
// SCOPE / FIDELITY (important):
//   * Routing is NOT the production `PeerRing::find_successor`. `successor_of`
//     returns the nearest forward node among `{me} ∪ connected` (spec-level,
//     single hop); production routes via `successors().min()` then
//     `finger.closest_predecessor`. The `Found -> Lookup` iteration models the
//     multi-hop refinement, but this is a routing *abstraction*, not the real fn.
//   * There is no `fix_fingers`/`FindSuccessorForFix` action in this discovery
//     abstraction. Stage 5 models its retry/resource state separately.
//   * It runs N=3, K=3, so every node's successor capacity spans all peers —
//     it does NOT reproduce the production regime behind the 6-node integration
//     flake (six clustered DIDs, K=3, successor truncation + high-index finger
//     fixes). So this DEMONSTRATES the order-sensitivity *mechanism*; it does not
//     formally model that specific 6-node configuration.
// ===================================================================

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum DMsg {
    /// Join lookup: "connect me, and tell me the successor of `origin`'s DID."
    /// The receiver answers from its current knowledge and registers `origin`.
    /// (A node always looks up its own DID, so no separate target is carried —
    /// keeping it out of the message shrinks the state space.)
    Lookup { origin: usize },
    /// Reply to a `Lookup`: `node` is the discovered successor to connect to.
    Found { node: usize },
    /// `QueryForTopoInfoSend`: the stabilizing node asks its successor head
    /// for the head's predecessor.
    QueryTopo,
    /// `QueryForTopoInfoReport`: the head's current predecessor, if any. The
    /// requester connects to it and then notifies the head.
    TopoReport { pred: Option<usize> },
    /// `NotifyPredecessorSend`: the sender proposes itself as predecessor.
    NotifyPred { from: usize },
}

/// The periodic stabilization tick.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum DTimer {
    Stabilize,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DState {
    /// Peers this node has connected to (its transport links / DHT knowledge).
    connected: BTreeSet<usize>,
    pred: Option<usize>,
    /// Stabilization rounds elapsed. Exhaustive liveness checking of a retry
    /// protocol over an accumulating network is not finite, so the periodic
    /// stabilize query is bounded (`DiscoveryNode::rounds`); the model then
    /// verifies a decidable claim about convergence *within that bound*.
    ticks: u8,
}

/// A node for the discovery model. `all` is the shared DID set; `rounds` is the
/// per-node stabilization budget (kept a field, not a const, so the
/// no-bounded-convergence test can sweep several bounds).
#[derive(Clone)]
struct DiscoveryNode {
    all: Vec<Did>,
    rounds: u8,
}

impl DiscoveryNode {
    /// The successor list `me` currently has, given its connected peers: the K
    /// nearest forward peers. Equals `PeerRing` after joining `connected` (the
    /// `spec` operators are proven equal to production in `test_dht_convergence`);
    /// computed directly so the model checker can expand states cheaply.
    fn successors(&self, me: usize, connected: &BTreeSet<usize>) -> Vec<usize> {
        let mut v: Vec<usize> = connected.iter().copied().filter(|&c| c != me).collect();
        v.sort_by_key(|&c| spec::dist(self.all[me], self.all[c]));
        v.truncate(K);
        v
    }

    /// A spec-level *abstraction* of `find_successor(target)` (NOT the
    /// production routing — see the stage-2 SCOPE note): the nearest forward node
    /// among `{me} ∪ connected`, single hop. The real `chord.rs` finger-table
    /// routing is approximated by the `Found` -> `Lookup` iteration, which
    /// converges to the same answer over `connected`.
    fn successor_of(&self, me: usize, connected: &BTreeSet<usize>, target: Did) -> usize {
        std::iter::once(me)
            .chain(connected.iter().copied())
            .min_by_key(|&n| spec::dist(target, self.all[n]))
            .unwrap()
    }

    /// Mirrors `PeerRing::notify`: predecessor becomes the candidate closer behind.
    fn notify(&self, me: usize, cur: Option<usize>, from: usize) -> usize {
        match cur {
            Some(p)
                if spec::dist(self.all[me], self.all[p])
                    >= spec::dist(self.all[me], self.all[from]) =>
            {
                p
            }
            _ => from,
        }
    }
}

impl Actor for DiscoveryNode {
    type Msg = DMsg;
    type State = DState;
    type Timer = DTimer;
    type Random = ();
    type Storage = ();

    fn on_start(&self, id: Id, _storage: &Option<()>, o: &mut Out<Self>) -> DState {
        let me = usize::from(id);
        // Star bootstrap: every spoke knows the hub (node 0); the hub starts
        // knowing nobody and learns spokes as their lookups arrive — so whether
        // a spoke discovers its true successor depends on the order the hub
        // processes joins, which is exactly the real connect-time race.
        let connected = if me == 0 {
            BTreeSet::new()
        } else {
            // Ask the hub for my successor (this also registers me with it).
            o.send(Id::from(0usize), DMsg::Lookup { origin: me });
            BTreeSet::from([0])
        };
        o.set_timer(DTimer::Stabilize, model_timeout());
        DState {
            connected,
            pred: None,
            ticks: 0,
        }
    }

    fn on_timeout(&self, id: Id, state: &mut Cow<DState>, _t: &DTimer, o: &mut Out<Self>) {
        // Bounded periodic stabilization: stop after `rounds` so the model is
        // finite (the network is consumed-on-delivery, not accumulating).
        if state.ticks >= self.rounds {
            return;
        }
        let me = usize::from(id);
        // `pre_stabilize`: one query to the successor head per round.
        if let Some(&head) = self.successors(me, &state.connected).first() {
            o.send(Id::from(head), DMsg::QueryTopo);
        }
        state.to_mut().ticks += 1;
        o.set_timer(DTimer::Stabilize, model_timeout());
    }

    fn on_msg(&self, id: Id, state: &mut Cow<DState>, src: Id, msg: DMsg, o: &mut Out<Self>) {
        let me = usize::from(id);
        match msg {
            DMsg::Lookup { origin } => {
                // Answer over CURRENT knowledge, THEN register origin — so the
                // answer reflects what we knew before this peer joined (the
                // connect-time race).
                let succ = self.successor_of(me, &state.connected, self.all[origin]);
                o.send(Id::from(origin), DMsg::Found { node: succ });
                if origin != me && !state.connected.contains(&origin) {
                    state.to_mut().connected.insert(origin);
                }
            }
            DMsg::Found { node } => {
                if node != me && !state.connected.contains(&node) {
                    state.to_mut().connected.insert(node);
                    // Iterate: register with the discovered node and refine.
                    o.send(Id::from(node), DMsg::Lookup { origin: me });
                }
            }
            DMsg::QueryTopo => {
                o.send(src, DMsg::TopoReport { pred: state.pred });
            }
            DMsg::TopoReport { pred } => {
                // The report's predecessor is a bounded connection candidate;
                // applying the report then notifies the head (`Notify(head)`).
                if let Some(pred) = pred {
                    if pred != me && !state.connected.contains(&pred) {
                        state.to_mut().connected.insert(pred);
                        o.send(Id::from(pred), DMsg::Lookup { origin: me });
                    }
                }
                o.send(src, DMsg::NotifyPred { from: me });
            }
            DMsg::NotifyPred { from } => {
                let new_pred = self.notify(me, state.pred, from);
                if state.pred != Some(new_pred) {
                    state.to_mut().pred = Some(new_pred);
                }
            }
        }
    }
}

/// The K nearest forward peers among `connected` — what `me`'s successor list
/// converges to. Computed without a `PeerRing` (the property runs per state).
fn succ_among(all: &[Did], me: Did, connected: &BTreeSet<usize>) -> Vec<Did> {
    let mut v: Vec<Did> = connected
        .iter()
        .map(|&i| all[i])
        .filter(|&d| d != me)
        .collect();
    v.sort_by_key(|&d| spec::dist(me, d));
    v.truncate(K);
    v
}

/// `Always`: connected/pred reference real, distinct peers.
fn d_wellformed(
    model: &ActorModel<DiscoveryNode, Cfg, ()>,
    st: &ActorModelState<DiscoveryNode>,
) -> bool {
    let all = &model.cfg.all;
    st.actor_states.iter().enumerate().all(|(i, s)| {
        s.connected.iter().all(|&c| c < all.len() && c != i)
            && s.pred.is_none_or(|p| p < all.len() && p != i)
    })
}

/// Convergence: every node has connected to its true successors and learned its
/// true predecessor (the `spec` fixpoint).
fn d_converged(
    model: &ActorModel<DiscoveryNode, Cfg, ()>,
    st: &ActorModelState<DiscoveryNode>,
) -> bool {
    let all = &model.cfg.all;
    (0..all.len()).all(|i| {
        let s = &st.actor_states[i];
        succ_among(all, all[i], &s.connected) == spec::successors(all, all[i])
            && s.pred.map(|p| all[p]) == spec::predecessor(all, all[i])
    })
}

fn discovery_model(all: Vec<Did>, rounds: u8) -> ActorModel<DiscoveryNode, Cfg, ()> {
    let actors: Vec<DiscoveryNode> = all
        .iter()
        .map(|_| DiscoveryNode {
            all: all.clone(),
            rounds,
        })
        .collect();
    // Consumed-on-delivery network (messages aren't retained): combined with the
    // bounded round count this keeps the state space finite. Reordering across
    // channels is still fully explored.
    //
    // NOTE on properties: we assert `Always` (safety) and `Sometimes`
    // (convergence is reachable). We deliberately do NOT assert `Eventually`
    // (convergence on *every* interleaving within `rounds`): Stateright shows it
    // is FALSE, and the counterexample is the whole point — see
    // `test_discovery_has_no_bounded_convergence`.
    ActorModel::new(Cfg { all }, ())
        .actors(actors)
        .init_network(Network::new_unordered_nonduplicating([]))
        .property(
            Expectation::Always,
            "connected/pred well-formed",
            d_wellformed,
        )
        .property(
            Expectation::Sometimes,
            "discovery converges (reachable)",
            d_converged,
        )
}

/// Finite production-transition model for finger retry and stabilization laws.
///
/// This stage owns its adversarial environment alphabet and checks the real
/// topology reducer directly instead of duplicating transition semantics.
mod finger_retry_model;
mod relay_custody_model;
mod storage_model;

#[cfg(test)]
mod tests {
    use super::storage_model::*;
    use super::*;
    use crate::consts::ENTRY_DATA_MAX_LEN;

    /// Build a fully-converged DHT for `node` (the production join/notify path).
    fn build_converged(node: Did, all: &[Did]) -> PeerRing {
        let dht = PeerRing::new_with_storage(node, K as u8, Box::new(MemStorage::new()));
        for &other in all {
            if other != node {
                dht.admit_connected(other, None).unwrap();
                dht.notify(other).unwrap();
            }
        }
        dht
    }

    /// `apply_notify` / `rectify_predecessor_in` (the model's topology-aware
    /// predecessor rule, `#1184`) must equal the production
    /// `PeerRing::notify_within_topology` across all candidate `(current, from)`
    /// states on `N = 2, 3, 4` rings, including when `successors` are populated
    /// (and across non-empty partial successor subsets that create interior
    /// `(local, head)` intervals).
    #[test]
    fn test_apply_notify_matches_peer_ring() {
        for n in 2..=4u64 {
            let all: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            let node = ChordNode { all: all.clone() };
            let candidates = std::iter::once(None).chain(all.iter().copied().map(Some));
            for &me in &all {
                let others: Vec<Did> = all.iter().copied().filter(|&d| d != me).collect();
                for current in candidates.clone() {
                    if current == Some(me) {
                        continue;
                    }
                    for &from in &all {
                        // 1. Oracle successor list populated (`ChordNode::apply_notify`).
                        let dht =
                            PeerRing::new_with_storage(me, K as u8, Box::new(MemStorage::new()));
                        for s in spec::successors(&all, me) {
                            dht.successors().update(s).unwrap();
                        }
                        *dht.lock_predecessor().unwrap() = current;
                        let real = dht.notify_within_topology(from).unwrap();
                        assert_eq!(
                            node.apply_notify(me, current, from),
                            real,
                            "apply_notify != PeerRing::notify_within_topology (n={n}, me={me}, cur={current:?}, from={from})"
                        );

                        // 2. Every partial successor subset (including gaps where `from` lies
                        // strictly inside `(me, head)`).
                        for mask in 0u32..(1 << others.len()) {
                            let dht_sub = PeerRing::new_with_storage(
                                me,
                                K as u8,
                                Box::new(MemStorage::new()),
                            );
                            for (bit, &peer) in others.iter().enumerate() {
                                if mask & (1 << bit) != 0 {
                                    dht_sub.successors().update(peer).unwrap();
                                }
                            }
                            *dht_sub.lock_predecessor().unwrap() = current;
                            let succ_list = dht_sub.successors().list().unwrap();
                            let state = crate::dht::topology::TopologyState::new(
                                me,
                                succ_list.clone(),
                                current,
                                Vec::new(),
                            );
                            assert_eq!(
                                crate::dht::topology::rectify_predecessor_in(&state, from),
                                dht_sub.notify_within_topology(from).unwrap(),
                                "rectify_predecessor_in != PeerRing::notify_within_topology (n={n}, me={me}, succ={succ_list:?}, cur={current:?}, from={from})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Trace-validation (operation conformance, Tier 1): the model's successor
    /// computation must equal the real `PeerRing` successor list after joining
    /// `connected` — for EVERY partial-knowledge subset, not just the converged
    /// full set. This is what justifies the stage-2 model using the fast
    /// spec-level successor instead of a real `PeerRing` per step: it is proven
    /// to agree with production on exactly the partial states the checker
    /// explores. (Routing / multi-hop `find_successor` is out of scope here — it
    /// stays a documented abstraction; see the stage-2 SCOPE note.)
    #[test]
    fn test_successors_match_peer_ring_on_partial_states() {
        let all: Vec<Did> = (0..4u64).map(|i| did_frac(i, 4)).collect();
        let node = DiscoveryNode {
            all: all.clone(),
            rounds: 1,
        };
        let n = all.len();
        for me in 0..n {
            let others: Vec<usize> = (0..n).filter(|&i| i != me).collect();
            for mask in 0u32..(1 << others.len()) {
                let connected: BTreeSet<usize> = others
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, &i)| i)
                    .collect();

                let model: Vec<Did> = node
                    .successors(me, &connected)
                    .into_iter()
                    .map(|i| all[i])
                    .collect();

                let dht = PeerRing::new_with_storage(all[me], K as u8, Box::new(MemStorage::new()));
                for &c in &connected {
                    let _ = dht.admit_connected(all[c], None);
                }
                let real = dht.successors().list().unwrap();

                assert_eq!(
                    model, real,
                    "successors disagree with PeerRing (me={me}, connected={connected:?})"
                );
            }
        }
    }

    /// The snapshot <-> PeerRing round-trip must be lossless: this is what lets
    /// the Stateright actor carry a hashable state yet run real chord operations.
    #[test]
    fn test_snapshot_round_trip_is_lossless() {
        for n in 2..=6u64 {
            let dids: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            for &node in &dids {
                let original = DhtSnapshot::capture(&build_converged(node, &dids));
                let restored = DhtSnapshot::capture(&original.restore());
                pretty_assertions::assert_eq!(restored, original, "round-trip lossy at {node}");
            }
        }
    }

    /// Stage 1: under EVERY message interleaving, the predecessor-update rule
    /// (notify to the full-mesh/oracle successor set) drives every node's
    /// predecessor to the `spec` fixpoint. This is the predecessor subprotocol,
    /// not successor discovery (see the SCOPE note on the stage-1 model).
    #[test]
    fn test_notify_predecessor_converges_under_full_mesh() {
        let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
        notify_model(all)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// Stage 1 (`#1184`): under full-mesh oracle successors plus adversarial/unilateral
    /// `NotifyPred` from `me`, `head`, and every `non_predecessor` on `N = 3, 4` rings,
    /// Stateright BFS proves:
    /// - `Always`: `predecessor` is never `me` and never `head`, and once set to the
    ///   true `spec::predecessor` no unilateral `NotifyPred` from any other ring member
    ///   can overwrite it;
    /// - `Eventually`: every node's predecessor still converges to `spec::predecessor`.
    #[test]
    fn test_notify_predecessor_rejects_head_and_interior_preemption_under_full_mesh() {
        for n in [3u64, 4] {
            let all: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            notify_model(all)
                .checker()
                .spawn_bfs()
                .join()
                .assert_properties();
        }
    }

    /// Stage 2 — safety + reachability. Over the whole interleaving graph of the
    /// star bootstrap, the discovery protocol never corrupts a node's state
    /// (`Always`) and *can* reach the full `spec` fixpoint (`Sometimes`).
    #[test]
    fn test_discovery_is_safe_and_can_converge() {
        let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
        discovery_model(all, 2)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// Stage 2 — the key (scoped) result. In this 3-node star abstraction, the
    /// protocol does NOT converge on every interleaving within a fixed number of
    /// stabilization rounds: a node can learn a peer (via that peer's join
    /// `Lookup`) only AFTER it has spent its round budget, so it never sends that
    /// peer the corrective `NotifyPred`, leaving the peer's predecessor at a
    /// suboptimal value. We check this for several bounds (a counterexample
    /// exists at each); the general "no fixed bound suffices" claim is the
    /// matching analytical argument (the adversary delays a node learning a peer
    /// until after its budget). Convergence therefore needs Chord's fairness
    /// assumption (every node stabilizes infinitely often) — consistent with the
    /// integration test's residual, order-sensitive flakiness. We assert the
    /// counterexample EXISTS rather than chase it away.
    #[test]
    fn test_discovery_has_no_bounded_convergence() {
        // Two bounds suffice as evidence; `rounds=3` blows the state space up
        // without adding signal. The general "no fixed bound" claim is the
        // analytical argument in the doc comment, not an exhaustive sweep.
        for rounds in [1u8, 2] {
            let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
            let actors: Vec<DiscoveryNode> = all
                .iter()
                .map(|_| DiscoveryNode {
                    all: all.clone(),
                    rounds,
                })
                .collect();
            let checker = ActorModel::new(Cfg { all }, ())
                .actors(actors)
                .init_network(Network::new_unordered_nonduplicating([]))
                .property(Expectation::Eventually, "bounded convergence", d_converged)
                .checker()
                .spawn_bfs()
                .join();
            assert!(
                checker.discovery("bounded convergence").is_some(),
                "expected a no-bounded-convergence counterexample at rounds={rounds} \
                 (a peer learned after a node's stabilization budget is never notified)"
            );
        }
    }

    /// Stage 3 — CRDT SEC law. Storage values are real finite [`Entry`]
    /// carriers; anti-entropy messages only deliver more joins.
    #[test]
    fn test_storage_entry_join_satisfies_semilattice_laws() {
        for carrier in storage_join_carriers() {
            let values = (0u8..8)
                .map(|bits| storage_value_from_bits(carrier, bits))
                .collect::<Vec<_>>();

            for left in &values {
                let idempotent = storage_join_entry(left.entry.clone(), left.entry.clone());
                assert_eq!(idempotent, left.entry);
                for right in &values {
                    let expected = storage_value_by_bits(&values, left.bits | right.bits);
                    let left_right = storage_join_entry(left.entry.clone(), right.entry.clone());
                    let right_left = storage_join_entry(right.entry.clone(), left.entry.clone());
                    assert_eq!(left_right, expected.entry);
                    assert_eq!(left_right, right_left);

                    for third in &values {
                        let left_assoc = storage_join_entry(
                            storage_join_entry(left.entry.clone(), right.entry.clone()),
                            third.entry.clone(),
                        );
                        let right_assoc = storage_join_entry(
                            left.entry.clone(),
                            storage_join_entry(right.entry.clone(), third.entry.clone()),
                        );
                        assert_eq!(left_assoc, right_assoc);
                    }
                }
            }
        }
    }

    /// Stage 3 — topology-aware SEC. Carrier safety is proved by
    /// `test_storage_entry_join_satisfies_semilattice_laws`; this model checks the
    /// topology-specific liveness obligation that every merged state reaches the
    /// global lub under fair repeated send/deliver. The finite carriers cover
    /// bounded top-N data, overwrite reset floors, and relay tombstones. The
    /// copy/ack/delete model below is only local cleanup safety.
    #[test]
    fn test_storage_join_topology_model_converges_after_partition_merge() {
        for scenario in storage_join_scenarios() {
            let global_lub = scenario.global_lub();
            for partition in STORAGE_PARTITION_MASKS {
                for state in reachable_storage_join_states(partition, scenario.initial.clone()) {
                    if state.phase == StorageJoinPhase::Merged {
                        let closed = state.drive_to_quiescent_lub(&global_lub);
                        assert!(
                            closed.is_quiescent_lub(&global_lub),
                            "merged state did not close to global lub for {}: start={state:?}, closed={closed:?}",
                            scenario.name
                        );
                    }
                }
            }
        }
    }

    /// Stage 3 (`#1179`) — Anti-Eviction Safety Property for `retain_fair_data_elements`.
    /// For every non-empty bitmask `bits` in `1..8`, whenever bit `0b001` (actor 1's flood of
    /// `ENTRY_DATA_MAX_LEN` high-timestamp elements), `0b010` (actor 2), or `0b100` (actor 3)
    /// is set in `bits`, that actor's newest element (`r = 0`) is preserved in
    /// `storage_value_from_bits(StorageJoinCarrier::DataFairMultiWriter, bits).entry.data`,
    /// proving that a flooding writer cannot evict honest writers' newest elements under any
    /// join order.
    #[test]
    fn test_fair_multi_writer_preserves_every_active_writer_newest_element() {
        let newest_a1 = storage_element(&format!("flood-a1-{}", ENTRY_DATA_MAX_LEN - 1));
        let newest_a2 = storage_element("honest-a2-1");
        let newest_a3 = storage_element("honest-a3-1");

        for bits in 1u8..8 {
            let value = storage_value_from_bits(StorageJoinCarrier::DataFairMultiWriter, bits);
            if bits & 0b001 != 0 {
                assert!(
                    value.entry.data.contains(&newest_a1),
                    "actor 1 newest element (r=0) missing at bits={bits:#05b}"
                );
            }
            if bits & 0b010 != 0 {
                assert!(
                    value.entry.data.contains(&newest_a2),
                    "actor 2 newest element (r=0) evicted by flood at bits={bits:#05b}"
                );
            }
            if bits & 0b100 != 0 {
                assert!(
                    value.entry.data.contains(&newest_a3),
                    "actor 3 newest element (r=0) evicted by flood at bits={bits:#05b}"
                );
            }
        }
    }

    /// Stage 4 — storage S2' and same-value ABA custody conservation (#614, #1207).
    /// Exhaustively explores the finite state graph for one placement hand-off across two
    /// concurrent hand-off slots, local writes, and return hand-offs, checking:
    /// 1. S2': deleting the local placement key is allowed only when the successor has durably
    ///    stored the same value;
    /// 2. Custody Conservation (#1207): in every reachable state, at least one of `local` or
    ///    `successor` holds a live copy (`local.is_some() || successor.is_some()`), including
    ///    across the same-value ABA sequence where `A` sends two hand-offs of `V0`, loses custody
    ///    on the first ack, receives `V0` back from `B`, and then receives the delayed second ack.
    #[test]
    fn test_storage_sync_model_preserves_no_update_loss() {
        let mut seen = BTreeSet::new();
        let mut frontier = vec![StorageSyncState::initial()];
        let mut saw_same_value_aba = false;
        while let Some(state) = frontier.pop() {
            if !seen.insert(state) {
                continue;
            }
            assert!(
                state.local.is_some() || state.successor.is_some(),
                "custody conservation violated (every live copy eliminated): {state:?}"
            );
            let steps = [
                StorageSyncStep::SendCopy(0),
                StorageSyncStep::SendCopy(1),
                StorageSyncStep::DeliverCopy(0),
                StorageSyncStep::DeliverCopy(1),
                StorageSyncStep::LocalWrite(StorageValue::V0),
                StorageSyncStep::LocalWrite(StorageValue::V1),
                StorageSyncStep::LocalWrite(StorageValue::V2),
                StorageSyncStep::ReturnCustodyToLocal,
                StorageSyncStep::DeliverAckDelete(0),
                StorageSyncStep::DeliverAckDelete(1),
            ];
            for step in steps {
                if let Some(next) = state.step(step) {
                    if let Some(removed) = state.removed_local_value(next) {
                        assert!(
                            next.successor.is_some_and(|stored| stored >= removed),
                            "S2' violated by {step:?}: {state:?} -> {next:?}"
                        );
                    }
                    if matches!(step, StorageSyncStep::DeliverAckDelete(1))
                        && state.local == Some(StorageValue::V0)
                        && state.local_incarnation == 1
                        && state.successor.is_none()
                        && state.ack_in_flight[1] == Some((StorageValue::V0, 0))
                    {
                        assert_eq!(next.local, Some(StorageValue::V0));
                        saw_same_value_aba = true;
                    }
                    if !seen.contains(&next) {
                        frontier.push(next);
                    }
                }
            }
        }
        assert!(
            saw_same_value_aba,
            "BFS must cover the same-value ABA sequence (#1207)"
        );
    }
}
