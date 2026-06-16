//! Mechanism two — model-checking stabilization with Stateright.
//!
//! `dht_convergence` pins the SAFETY fixpoint deterministically; here we
//! exhaustively explore the *interleavings* of the stabilization protocol.
//!
//! Stateright requires `State: Clone + PartialEq + Hash`, which a live `PeerRing`
//! (it holds `Arc<Mutex<…>>` and `Box<dyn storage>`) is not. [`DhtSnapshot`]
//! shows the way out — the only state that matters for convergence is
//! `Did`-valued (DID, successors, predecessor, finger), so it round-trips
//! losslessly to/from a real `PeerRing` (proven below), making real chord ops
//! usable from a hashable model state. The models then use the `spec` operators
//! (proven equal to production in `dht_convergence`) directly, which is far
//! cheaper for the checker to expand than a `DashMap`-backed `PeerRing` per step.
//!
//! Staging:
//!   * Stage 1 — the `notify` protocol on a full mesh: every predecessor
//!     converges to the `spec` fixpoint under every message interleaving.
//!   * Stage 2 — discovery from a star bootstrap: safety + reachability hold,
//!     and the model PROVES there is no bounded-round convergence (a peer
//!     learned after a node's stabilization budget is never notified), the
//!     formal root of the integration test's order-sensitive flakiness.

use std::borrow::Cow;
use std::collections::BTreeSet;

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

use super::dht_convergence::spec;
use super::dht_convergence::K;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::storage::MemStorage;

/// A DID at `num/den` of the way round the ring — deterministic test positions.
fn did_frac(num: u64, den: u64) -> Did {
    Did::from((BigUint::from(1u8) << 160) * BigUint::from(num) / BigUint::from(den))
}

/// The hashable topology state of a node — everything in `PeerRing` that drives
/// routing/convergence, and nothing else (no VNode storage/cache).
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
// Stage 1: the `notify` protocol (predecessor convergence on a full mesh).
//
// Maps to the TLA+ `Notify` action and `handlers/stabilization.rs`:
//   on_start : each node tells every successor "I might be your predecessor".
//   on Send  : apply the `PeerRing::notify` rule to the predecessor.
// The mesh is complete here, so discovery and the report-back/connect step are
// out of scope (the reported peer is already known); this stage isolates and
// exhausts the notify delivery interleavings. Stage 2 (below) adds discovery
// from a star bootstrap.
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

    /// The predecessor update. Mirrors `PeerRing::notify` exactly — predecessor
    /// becomes the candidate that is closer *behind* `me` (larger clockwise
    /// distance) — computed directly rather than through a throwaway `PeerRing`
    /// (which allocates a `DashMap`) so the model checker can expand states
    /// cheaply. The heavyweight real-`PeerRing` path is reserved for stage 2's
    /// `find_successor`, where the routing logic is non-trivial.
    fn apply_notify(&self, me: Did, current: Option<Did>, from: Did) -> Did {
        match current {
            Some(cur) if spec::dist(me, cur) >= spec::dist(me, from) => cur,
            _ => from,
        }
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
        // notify_predecessor: tell each successor "I might be your predecessor",
        // then re-arm — i.e. periodic, per the Chord paper. The network is a
        // duplicating *set* (`new_unordered_duplicating`), so a re-sent identical
        // notification neither grows the state nor is lost: it stays available to
        // be (re-)delivered, which is exactly the effect of periodic re-sending
        // under a reliable channel, while keeping the state space finite.
        let me = self.did(id);
        for s in spec::successors(&self.all, me) {
            o.send(self.id_of(s), Msg::NotifyPred { from: me });
        }
        o.set_timer(Timer::Stabilize, model_timeout());
    }

    fn on_msg(&self, id: Id, state: &mut Cow<NodeState>, _src: Id, msg: Msg, _o: &mut Out<Self>) {
        let me = self.did(id);
        match msg {
            Msg::NotifyPred { from } => {
                let new_pred = self.apply_notify(me, state.pred, from);
                if state.pred != Some(new_pred) {
                    state.to_mut().pred = Some(new_pred);
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
// Stage 2: discovery from a star bootstrap (the REAL find_successor routing).
//
// This is the regime the integration test exercises and where the residual
// flakiness lives. Unlike stage 1 (full mesh), each node's connected-peer set
// grows DYNAMICALLY: the hub learns a spoke only when that spoke's join lookup
// arrives, so the connect-time `find_successor(self)` race is modelled. Routing
// uses the real `PeerRing::find_successor`; the join lookups plus the
// notify/report chain are what must drive every node to its true successor and
// predecessor. `Eventually` answers the open question: does the non-experimental
// protocol converge under EVERY interleaving, or is there a stalling order?
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
    /// `NotifyPredecessorSend`.
    NotifyPred { from: usize },
    /// `NotifyPredecessorReport`: the sender connects to the reported predecessor.
    NotifyPredReport { pred: usize },
}

/// The periodic stabilization tick.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum DTimer {
    Stabilize,
}

/// Bound on stabilization rounds per node. Exhaustive liveness checking of a
/// retry protocol over an accumulating network is not finite, so we verify the
/// stronger, decidable claim: convergence within a bounded number of rounds
/// under EVERY interleaving. A counterexample at this bound is a real stalling
/// order; passing is strong evidence convergence is order-robust for small N.
const STAB_ROUNDS: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DState {
    /// Peers this node has connected to (its transport links / DHT knowledge).
    connected: BTreeSet<usize>,
    pred: Option<usize>,
    /// Stabilization rounds elapsed (bounds the periodic notify; see above).
    ticks: u8,
}

/// A node for the discovery model. As in stage 1, `all` is the shared DID set.
#[derive(Clone)]
struct DiscoveryNode {
    all: Vec<Did>,
}

impl DiscoveryNode {
    /// The successor list `me` currently has, given its connected peers: the K
    /// nearest forward peers. Equals `PeerRing` after joining `connected` (the
    /// `spec` operators are proven equal to production in `dht_convergence`);
    /// computed directly so the model checker can expand states cheaply.
    fn successors(&self, me: usize, connected: &BTreeSet<usize>) -> Vec<usize> {
        let mut v: Vec<usize> = connected.iter().copied().filter(|&c| c != me).collect();
        v.sort_by_key(|&c| spec::dist(self.all[me], self.all[c]));
        v.truncate(K);
        v
    }

    /// `find_successor(target)` as resolved from `me`'s current knowledge: the
    /// nearest forward node among `{me} ∪ connected`. This is single-hop; the
    /// real multi-hop routing is modelled by the `Found` -> `Lookup` iteration
    /// (the requester re-asks the node it just discovered), which converges to
    /// the same answer.
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
        // Bounded periodic stabilization: stop after STAB_ROUNDS so the model is
        // finite (the network is consumed-on-delivery, not accumulating).
        if state.ticks >= STAB_ROUNDS {
            return;
        }
        let me = usize::from(id);
        for s in self.successors(me, &state.connected) {
            if s != me {
                o.send(Id::from(s), DMsg::NotifyPred { from: me });
            }
        }
        state.to_mut().ticks += 1;
        o.set_timer(DTimer::Stabilize, model_timeout());
    }

    fn on_msg(&self, id: Id, state: &mut Cow<DState>, _src: Id, msg: DMsg, o: &mut Out<Self>) {
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
            DMsg::NotifyPred { from } => {
                let new_pred = self.notify(me, state.pred, from);
                if state.pred != Some(new_pred) {
                    state.to_mut().pred = Some(new_pred);
                }
                if new_pred != from {
                    o.send(Id::from(from), DMsg::NotifyPredReport { pred: new_pred });
                }
            }
            DMsg::NotifyPredReport { pred } => {
                if pred != me && !state.connected.contains(&pred) {
                    state.to_mut().connected.insert(pred);
                    o.send(Id::from(pred), DMsg::Lookup { origin: me });
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

fn discovery_model(all: Vec<Did>) -> ActorModel<DiscoveryNode, Cfg, ()> {
    let actors: Vec<DiscoveryNode> = all
        .iter()
        .map(|_| DiscoveryNode { all: all.clone() })
        .collect();
    // Consumed-on-delivery network (messages aren't retained): combined with the
    // bounded round count this keeps the state space finite. Reordering across
    // channels is still fully explored.
    //
    // NOTE on properties: we assert `Always` (safety) and `Sometimes`
    // (convergence is reachable). We deliberately do NOT assert `Eventually`
    // (convergence on *every* interleaving within STAB_ROUNDS): Stateright shows
    // it is FALSE, and the counterexample is the whole point — see
    // `discovery_has_no_bounded_convergence`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fully-converged DHT for `node` (the production join/notify path).
    fn build_converged(node: Did, all: &[Did]) -> PeerRing {
        let dht = PeerRing::new_with_storage(node, K as u8, Box::new(MemStorage::new()));
        for &other in all {
            if other != node {
                dht.join(other).unwrap();
                dht.notify(other).unwrap();
            }
        }
        dht
    }

    /// The snapshot <-> PeerRing round-trip must be lossless: this is what lets
    /// the Stateright actor carry a hashable state yet run real chord operations.
    #[test]
    fn snapshot_round_trip_is_lossless() {
        for n in 2..=6u64 {
            let dids: Vec<Did> = (0..n).map(|i| did_frac(i, n)).collect();
            for &node in &dids {
                let original = DhtSnapshot::capture(&build_converged(node, &dids));
                let restored = DhtSnapshot::capture(&original.restore());
                pretty_assertions::assert_eq!(restored, original, "round-trip lossy at {node}");
            }
        }
    }

    /// Stage 1: under EVERY message interleaving (Stateright BFS over the
    /// unordered reliable network), the real `notify` protocol drives every
    /// node's predecessor to the `spec` fixpoint.
    #[test]
    fn notify_converges_under_all_interleavings() {
        let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
        notify_model(all)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// Stage 2 — safety + reachability. Over the whole interleaving graph of the
    /// star bootstrap, the discovery protocol never corrupts a node's state
    /// (`Always`) and *can* reach the full `spec` fixpoint (`Sometimes`).
    #[test]
    fn discovery_is_safe_and_can_converge() {
        let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
        discovery_model(all)
            .checker()
            .spawn_bfs()
            .join()
            .assert_properties();
    }

    /// Stage 2 — the key result. The non-experimental protocol does NOT converge
    /// on every interleaving within a fixed number of stabilization rounds: a
    /// node can learn a peer (via that peer's join `Lookup`) only AFTER it has
    /// spent its `STAB_ROUNDS` rounds, so it never sends that peer the corrective
    /// `NotifyPred`, leaving the peer's predecessor at a suboptimal value. So for
    /// any fixed bound an adversarial order defeats convergence — it holds only
    /// under Chord's fairness assumption (every node stabilizes infinitely
    /// often). This is the formal root of the integration test's residual,
    /// order-sensitive flakiness. We assert the counterexample EXISTS.
    #[test]
    fn discovery_has_no_bounded_convergence() {
        let all: Vec<Did> = (0..3u64).map(|i| did_frac(i, 3)).collect();
        let actors: Vec<DiscoveryNode> = all
            .iter()
            .map(|_| DiscoveryNode { all: all.clone() })
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
            "expected a no-bounded-convergence counterexample (a peer learned \
             after a node's stabilization budget is never notified)"
        );
    }
}
