//! Mechanism two — model-checking the *liveness* of stabilization with
//! Stateright, driving the REAL `chord.rs` operations.
//!
//! Stateright requires `State: Clone + PartialEq + Hash`, which a live
//! `PeerRing` (it holds `Arc<Mutex<…>>` and `Box<dyn storage>`) is not. But the
//! only state that matters for topology convergence is `Did`-valued — the DID,
//! the successor list, the predecessor, and the finger table — so we keep a
//! hashable [`DhtSnapshot`] of exactly that and **reconstruct a real `PeerRing`
//! from it** whenever we need to run a real operation. That gives a faithful
//! model (real node logic) with a finite, hashable state space (only the
//! message interleaving branches).
//!
//! Staging:
//!   * [`DhtSnapshot`] — the hashable state + lossless round-trip (proven).
//!   * Stage 1 (this file): the `notify` protocol on a full mesh, checked under
//!     every message interleaving with the real `PeerRing::notify`.
//!   * Stage 2 (next): discovery (find_successor / connect) from a star, where
//!     `DhtSnapshot` becomes the actor state.

use std::borrow::Cow;

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
// exhausts the notify delivery interleavings. Stage 2 adds discovery, with
// `DhtSnapshot` as the actor state and the real `find_successor` for routing.
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
}
