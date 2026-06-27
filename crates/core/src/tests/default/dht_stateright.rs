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

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::hash::Hash;
use std::hash::Hasher;

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
use crate::algebra::JoinSemilattice;
use crate::consts::ENTRY_DATA_MAX_LEN;
use crate::dht::entry::Entry;
use crate::dht::entry::EntryCrdt;
use crate::dht::entry::EntryDot;
use crate::dht::entry::EntryKind;
use crate::dht::entry::EntryVersion;
use crate::dht::successor::SuccessorReader;
use crate::dht::successor::SuccessorWriter;
use crate::dht::Chord;
use crate::dht::Did;
use crate::dht::PeerRing;
use crate::message::Encoded;
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
// Production `Stabilizer::notify_predecessor` instead sends to the node's
// current, possibly stale/incomplete local successor list. So this stage does
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

    /// The predecessor update — a direct reimplementation of `PeerRing::notify`
    /// (predecessor becomes the candidate closer *behind* `me`), used instead of
    /// a throwaway `PeerRing` (which allocates a `DashMap`) so the checker can
    /// expand states cheaply. It is NOT assumed equivalent: the test
    /// `apply_notify_matches_peer_ring` checks it against the real
    /// `PeerRing::notify` across representative states, so it remains a faithful
    /// regression guard even though the model doesn't call the production fn.
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
// Stage 2: successor discovery from a star bootstrap — a small ABSTRACTION.
//
// Unlike stage 1, each node's connected-peer set grows DYNAMICALLY: the hub
// learns a spoke only when that spoke's join lookup arrives, so the connect-time
// `find_successor(self)` race is modelled, and the join lookups + notify/report
// chain must drive discovery.
//
// SCOPE / FIDELITY (important):
//   * Routing is NOT the production `PeerRing::find_successor`. `successor_of`
//     returns the nearest forward node among `{me} ∪ connected` (spec-level,
//     single hop); production routes via `successors().min()` then
//     `finger.closest_predecessor`. The `Found -> Lookup` iteration models the
//     multi-hop refinement, but this is a routing *abstraction*, not the real fn.
//   * There is no `fix_fingers`/`FindSuccessorForFix` action.
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct DState {
    /// Peers this node has connected to (its transport links / DHT knowledge).
    connected: BTreeSet<usize>,
    pred: Option<usize>,
    /// Stabilization rounds elapsed. Exhaustive liveness checking of a retry
    /// protocol over an accumulating network is not finite, so the periodic
    /// notify is bounded (`DiscoveryNode::rounds`); the model then verifies a
    /// decidable claim about convergence *within that bound*.
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
    /// `spec` operators are proven equal to production in `dht_convergence`);
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

// ===================================================================
// Stage 3: storage CRDT SEC topology model.
//
// State variables:
//   phase    in {Partitioned, Merged}
//   replica  in StorageJoinValue^3
//
// Initial state:
//   replicas start with independent local writes A, B, C.
//   phase = Partitioned, where only same-side nodes may exchange state.
//
// Next-state relation:
//   Transfer(from, to) applies replica[to] := replica[to] join replica[from]
//   whenever the topology allows that edge.
//   Merge changes phase from Partitioned to Merged, enabling every edge.
//
// Carrier safety:
//   `storage_entry_join_satisfies_semilattice_laws` proves the real Entry
//   carriers are join-semilattices over this finite domain. This topology
//   model therefore does not duplicate the carrier <= LUB invariant; its
//   distinct obligation is the liveness/closure step below.
//
// Liveness expectation under fair anti-entropy:
//   from any reachable Merged state, repeated Transfer steps reach the single
//   least upper bound at every replica.
//
// Refinement:
//   An asynchronous send/deliver trace projects to this Transfer model because
//   delivery is a pure join of a sender snapshot into the receiver. Message
//   reordering and duplication are covered by the semilattice law checked
//   below: join is commutative and idempotent.
//
// Quotient:
//   `StorageJoinValue` hashes and compares only `(carrier, bits)` so BFS stays
//   finite. The test `storage_entry_join_satisfies_semilattice_laws` is the
//   refinement witness: for every finite carrier state, real Entry::join equals
//   canonical(bits_a union bits_b). The topology model is therefore checked on
//   the quotient, while carrier correctness is checked on the real entries.
// ===================================================================

const STORAGE_REPLICA_COUNT: usize = 3;
const STORAGE_PARTITION_MASKS: [StoragePartition; STORAGE_REPLICA_COUNT] = [
    StoragePartition(0b001),
    StoragePartition(0b010),
    StoragePartition(0b011),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct StoragePartition(u8);

impl StoragePartition {
    fn permits(self, from: usize, to: usize) -> bool {
        if from == to {
            return false;
        }
        self.side(from) == self.side(to)
    }

    fn side(self, node: usize) -> bool {
        let shift = match u32::try_from(node) {
            Ok(shift) => shift,
            Err(_) => return false,
        };
        let Some(bit) = 1u8.checked_shl(shift) else {
            return false;
        };
        self.0 & bit != 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum StorageJoinCarrier {
    DataBoundedTopN,
    DataOverwriteReset,
    RelayTombstone,
}

#[derive(Clone, Debug)]
struct StorageJoinValue {
    carrier: StorageJoinCarrier,
    bits: u8,
    entry: Entry,
}

impl StorageJoinValue {
    fn new(carrier: StorageJoinCarrier, bits: u8, entry: Entry) -> Self {
        match entry.try_into_storage_entry() {
            Ok(entry) => Self {
                carrier,
                bits,
                entry,
            },
            Err(error) => panic!("storage model entry must normalize: {error}"),
        }
    }

    fn bottom_like(&self) -> Self {
        storage_value_from_bits(self.carrier, 0)
    }
}

impl PartialEq for StorageJoinValue {
    fn eq(&self, other: &Self) -> bool {
        self.carrier == other.carrier && self.bits == other.bits
    }
}

impl Eq for StorageJoinValue {}

impl Hash for StorageJoinValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.carrier.hash(state);
        self.bits.hash(state);
    }
}

impl Ord for StorageJoinValue {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.carrier
            .cmp(&other.carrier)
            .then_with(|| self.bits.cmp(&other.bits))
    }
}

impl PartialOrd for StorageJoinValue {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl JoinSemilattice for StorageJoinValue {
    fn join(self, other: Self) -> Self {
        if self.carrier != other.carrier {
            panic!("storage model joins only one carrier");
        }
        let carrier = self.carrier;
        let bits = self.bits | other.bits;
        let joined = storage_join_entry(self.entry, other.entry);
        Self::new(carrier, bits, joined)
    }
}

struct StorageJoinScenario {
    name: &'static str,
    initial: [StorageJoinValue; STORAGE_REPLICA_COUNT],
}

impl StorageJoinScenario {
    fn bottom(&self) -> StorageJoinValue {
        self.initial[0].bottom_like()
    }

    fn global_lub(&self) -> StorageJoinValue {
        self.initial
            .iter()
            .cloned()
            .fold(self.bottom(), JoinSemilattice::join)
    }
}

fn storage_model_did(offset: u32) -> Did {
    Did::from(10_000u32.saturating_add(offset))
}

fn storage_version(time: u128, actor: u32, operation: u32) -> EntryVersion {
    EntryVersion::new(time, Did::from(actor), Did::from(operation))
}

fn storage_index(index: usize) -> u32 {
    match u32::try_from(index) {
        Ok(index) => index,
        Err(_) => panic!("storage model index must fit in u32"),
    }
}

fn storage_dot(version: EntryVersion, index: usize) -> EntryDot {
    let index = storage_index(index);
    EntryDot { version, index }
}

fn storage_encoded(label: &str) -> Encoded {
    Encoded::from(label)
}

fn storage_join_entry(left: Entry, right: Entry) -> Entry {
    let joined = match left.join(right) {
        Ok(entry) => entry,
        Err(error) => panic!("storage model joins only compatible entries: {error}"),
    };
    match joined.try_into_storage_entry() {
        Ok(entry) => entry,
        Err(error) => panic!("storage model join result must normalize: {error}"),
    }
}

fn data_value_range(did: Did, label: &'static str, start_time: u128, count: usize) -> Entry {
    let data = (0..count)
        .map(|index| storage_encoded(&format!("{label}-{index}")))
        .collect::<Vec<_>>();
    let dots = (0..count)
        .map(|offset| {
            let index = storage_index(offset);
            let time = start_time.saturating_add(u128::from(index));
            storage_dot(
                storage_version(time, 1, 1_000u32.saturating_add(index)),
                offset,
            )
        })
        .collect::<Vec<_>>();
    Entry {
        did,
        data,
        kind: EntryKind::Data,
        crdt: EntryCrdt {
            register: None,
            dots,
            tombstones: Vec::new(),
        },
    }
}

fn data_overwrite_value(did: Did, label: &'static str, version: EntryVersion) -> Entry {
    Entry {
        did,
        data: vec![storage_encoded(label)],
        kind: EntryKind::Data,
        crdt: EntryCrdt {
            register: Some(version),
            dots: vec![storage_dot(version, 0)],
            tombstones: Vec::new(),
        },
    }
}

fn relay_add_value(did: Did, label: &'static str, dot: EntryDot) -> Entry {
    Entry {
        did,
        data: vec![storage_encoded(label)],
        kind: EntryKind::RelayMessage,
        crdt: EntryCrdt {
            register: None,
            dots: vec![dot],
            tombstones: Vec::new(),
        },
    }
}

fn relay_remove_value(did: Did, dot: EntryDot) -> Entry {
    Entry {
        did,
        data: Vec::new(),
        kind: EntryKind::RelayMessage,
        crdt: EntryCrdt {
            register: None,
            dots: Vec::new(),
            tombstones: vec![dot],
        },
    }
}

fn storage_delta_entry(carrier: StorageJoinCarrier, bit: u8) -> Entry {
    match carrier {
        StorageJoinCarrier::DataBoundedTopN => data_value_range(
            storage_model_did(1),
            match bit {
                0b001 => "low",
                0b010 => "mid",
                0b100 => "high",
                _ => panic!("storage model delta bit must be singleton"),
            },
            match bit {
                0b001 => 1,
                0b010 => 1_000,
                0b100 => 2_000,
                _ => panic!("storage model delta bit must be singleton"),
            },
            ENTRY_DATA_MAX_LEN,
        ),
        StorageJoinCarrier::DataOverwriteReset => match bit {
            0b001 => data_value_range(storage_model_did(2), "stale-a", 1, 3),
            0b010 => {
                data_overwrite_value(storage_model_did(2), "reset", storage_version(100, 2, 200))
            }
            0b100 => data_value_range(storage_model_did(2), "stale-c", 10, 3),
            _ => panic!("storage model delta bit must be singleton"),
        },
        StorageJoinCarrier::RelayTombstone => {
            let relay_a_dot = storage_dot(storage_version(1, 1, 10), 0);
            let relay_b_dot = storage_dot(storage_version(2, 2, 20), 0);
            match bit {
                0b001 => relay_add_value(storage_model_did(3), "relay-a", relay_a_dot),
                0b010 => relay_add_value(storage_model_did(3), "relay-b", relay_b_dot),
                0b100 => relay_remove_value(storage_model_did(3), relay_a_dot),
                _ => panic!("storage model delta bit must be singleton"),
            }
        }
    }
}

fn storage_bottom_entry(carrier: StorageJoinCarrier) -> Entry {
    let (did, kind) = match carrier {
        StorageJoinCarrier::DataBoundedTopN => (storage_model_did(1), EntryKind::Data),
        StorageJoinCarrier::DataOverwriteReset => (storage_model_did(2), EntryKind::Data),
        StorageJoinCarrier::RelayTombstone => (storage_model_did(3), EntryKind::RelayMessage),
    };
    Entry::new(did, Vec::new(), kind)
}

fn storage_value_from_bits(carrier: StorageJoinCarrier, bits: u8) -> StorageJoinValue {
    let mut entry = storage_bottom_entry(carrier);
    for bit in [0b001, 0b010, 0b100] {
        if bits & bit != 0 {
            entry = storage_join_entry(entry, storage_delta_entry(carrier, bit));
        }
    }
    StorageJoinValue::new(carrier, bits, entry)
}

fn storage_join_scenarios() -> Vec<StorageJoinScenario> {
    vec![
        StorageJoinScenario {
            name: "data bounded top-n",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b001),
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b010),
                storage_value_from_bits(StorageJoinCarrier::DataBoundedTopN, 0b100),
            ],
        },
        StorageJoinScenario {
            name: "data overwrite reset floor",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b001),
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b010),
                storage_value_from_bits(StorageJoinCarrier::DataOverwriteReset, 0b100),
            ],
        },
        StorageJoinScenario {
            name: "relay tombstone prevents resurrection",
            initial: [
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b001),
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b010),
                storage_value_from_bits(StorageJoinCarrier::RelayTombstone, 0b100),
            ],
        },
    ]
}

fn storage_join_carriers() -> [StorageJoinCarrier; 3] {
    [
        StorageJoinCarrier::DataBoundedTopN,
        StorageJoinCarrier::DataOverwriteReset,
        StorageJoinCarrier::RelayTombstone,
    ]
}

fn storage_value_by_bits(values: &[StorageJoinValue], bits: u8) -> &StorageJoinValue {
    match values.get(usize::from(bits)) {
        Some(value) => value,
        None => panic!("storage model bitmask must be in the finite carrier"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum StorageJoinPhase {
    Partitioned,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct StorageJoinState {
    partition: StoragePartition,
    phase: StorageJoinPhase,
    replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
}

impl StorageJoinState {
    fn initial(
        partition: StoragePartition,
        replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
    ) -> Self {
        Self {
            partition,
            phase: StorageJoinPhase::Partitioned,
            replicas,
        }
    }

    fn topology_permits(&self, from: usize, to: usize) -> bool {
        if from == to {
            return false;
        }
        match self.phase {
            StorageJoinPhase::Partitioned => self.partition.permits(from, to),
            StorageJoinPhase::Merged => from < STORAGE_REPLICA_COUNT && to < STORAGE_REPLICA_COUNT,
        }
    }

    fn transfer_current(&self, from: usize, to: usize) -> Option<Self> {
        if !self.topology_permits(from, to) {
            return None;
        }
        let value = self.replicas.get(from).cloned()?;
        let mut next = self.clone();
        let replica = next.replicas.get_mut(to)?;
        *replica = replica.clone().join(value);
        Some(next)
    }

    fn merge_partition(&self) -> Option<Self> {
        if self.phase == StorageJoinPhase::Merged {
            return None;
        }
        Some(Self {
            phase: StorageJoinPhase::Merged,
            ..self.clone()
        })
    }

    fn successors(&self) -> Vec<Self> {
        let mut next = Vec::new();
        if let Some(merged) = self.merge_partition() {
            next.push(merged);
        }
        for from in 0..STORAGE_REPLICA_COUNT {
            for to in 0..STORAGE_REPLICA_COUNT {
                if let Some(transferred) = self.transfer_current(from, to) {
                    next.push(transferred);
                }
            }
        }
        next
    }

    fn is_quiescent_lub(&self, global_lub: &StorageJoinValue) -> bool {
        self.replicas.iter().all(|value| value == global_lub)
    }

    fn transfer_all_current(&self) -> Self {
        let mut state = self.clone();
        for from in 0..STORAGE_REPLICA_COUNT {
            for to in 0..STORAGE_REPLICA_COUNT {
                if let Some(next) = state.transfer_current(from, to) {
                    state = next;
                }
            }
        }
        state
    }

    fn drive_to_quiescent_lub(&self, global_lub: &StorageJoinValue) -> Self {
        let mut state = self.clone();
        for _ in 0..STORAGE_REPLICA_COUNT * 8 {
            if state.is_quiescent_lub(global_lub) {
                return state;
            }
            let next = state.transfer_all_current();
            if next == state {
                return next;
            }
            state = next;
        }
        state
    }
}

fn reachable_storage_join_states(
    partition: StoragePartition,
    replicas: [StorageJoinValue; STORAGE_REPLICA_COUNT],
) -> BTreeSet<StorageJoinState> {
    let mut seen = BTreeSet::new();
    let mut frontier = vec![StorageJoinState::initial(partition, replicas)];
    while let Some(state) = frontier.pop() {
        if !seen.insert(state.clone()) {
            continue;
        }
        for next in state.successors() {
            if !seen.contains(&next) {
                frontier.push(next);
            }
        }
    }
    seen
}

// ===================================================================
// Stage 4: storage hand-off cleanup safety for one placement key.
//
// SCOPE: this is the #614 S2' cleanup model, not the storage convergence
// theorem. Convergence is Stage 3's join-semilattice fact. This stage abstracts
// exactly one placement key, the copy -> ack -> delete hand-off, and arbitrary
// local writes over a finite representative value domain while a copy or ack is
// in flight:
//
//   local(v) --SendCopy(v)--> copy_in_flight(v)
//   copy_in_flight(v) --DeliverCopy--> successor(v) + ack_in_flight(v)
//   local(v) --LocalWrite(w)--> local(w)
//   ack_in_flight(v) --DeliverAckDelete--> delete local only if local == v
//
// Property checked below:
//
//   Always S2': local(k) is removed only if successor(k) contains the same
//   value at the moment of removal.
// ===================================================================

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum StorageSyncStep {
    SendCopy,
    DeliverCopy,
    LocalWrite(StorageValue),
    DeliverAckDelete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum StorageValue {
    V0,
    V1,
    V2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct StorageSyncState {
    local: Option<StorageValue>,
    successor: Option<StorageValue>,
    copy_in_flight: Option<StorageValue>,
    ack_in_flight: Option<StorageValue>,
}

impl StorageSyncState {
    fn initial() -> Self {
        Self {
            local: Some(StorageValue::V0),
            successor: None,
            copy_in_flight: None,
            ack_in_flight: None,
        }
    }

    fn step(self, step: StorageSyncStep) -> Option<Self> {
        match step {
            StorageSyncStep::SendCopy => Some(Self {
                copy_in_flight: Some(self.local?),
                ..self
            }),
            StorageSyncStep::DeliverCopy => {
                let copied = self.copy_in_flight?;
                Some(Self {
                    successor: Some(copied),
                    copy_in_flight: None,
                    ack_in_flight: Some(copied),
                    ..self
                })
            }
            StorageSyncStep::LocalWrite(value)
                if self.copy_in_flight.is_some() || self.ack_in_flight.is_some() =>
            {
                Some(Self {
                    local: Some(value),
                    ..self
                })
            }
            StorageSyncStep::DeliverAckDelete => {
                let acked = self.ack_in_flight?;
                let local = match self.local {
                    Some(current) if current == acked => None,
                    current => current,
                };
                Some(Self {
                    local,
                    ack_in_flight: None,
                    ..self
                })
            }
            _ => None,
        }
    }

    fn removed_local_value(self, next: Self) -> Option<StorageValue> {
        match (self.local, next.local) {
            (Some(removed), None) => Some(removed),
            _ => None,
        }
    }
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

    /// `apply_notify` (the model's predecessor rule) must equal the production
    /// `PeerRing::notify` on every representative (current, from) state, so the
    /// model stays a regression guard even though it doesn't call the real fn.
    #[test]
    fn apply_notify_matches_peer_ring() {
        let all: Vec<Did> = (0..4u64).map(|i| did_frac(i, 4)).collect();
        let node = ChordNode { all: all.clone() };
        let candidates = std::iter::once(None).chain(all.iter().copied().map(Some));
        for &me in &all {
            for current in candidates.clone() {
                if current == Some(me) {
                    continue;
                }
                for &from in all.iter().filter(|&&d| d != me) {
                    let dht = PeerRing::new_with_storage(me, K as u8, Box::new(MemStorage::new()));
                    *dht.lock_predecessor().unwrap() = current;
                    let real = dht.notify(from).unwrap();
                    assert_eq!(
                        node.apply_notify(me, current, from),
                        real,
                        "apply_notify != PeerRing::notify (me={me}, cur={current:?}, from={from})"
                    );
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
    fn successors_match_peer_ring_on_partial_states() {
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
                    let _ = dht.join(all[c]);
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

    /// Stage 1: under EVERY message interleaving, the predecessor-update rule
    /// (notify to the full-mesh/oracle successor set) drives every node's
    /// predecessor to the `spec` fixpoint. This is the predecessor subprotocol,
    /// not successor discovery (see the SCOPE note on the stage-1 model).
    #[test]
    fn notify_predecessor_converges_under_full_mesh() {
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
    fn discovery_has_no_bounded_convergence() {
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
    fn storage_entry_join_satisfies_semilattice_laws() {
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
    /// `storage_entry_join_satisfies_semilattice_laws`; this model checks the
    /// topology-specific liveness obligation that every merged state reaches the
    /// global lub under fair repeated send/deliver. The finite carriers cover
    /// bounded top-N data, overwrite reset floors, and relay tombstones. The
    /// copy/ack/delete model below is only local cleanup safety.
    #[test]
    fn storage_join_topology_model_converges_after_partition_merge() {
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

    /// Stage 4 — storage S2'. Exhaustively explores the finite state graph for
    /// one placement hand-off and checks the cleanup safety predicate:
    /// deleting the local placement key is allowed only when the successor has
    /// durably stored the same value.
    #[test]
    fn storage_sync_model_preserves_no_update_loss() {
        let mut seen = BTreeSet::new();
        let mut frontier = vec![StorageSyncState::initial()];
        while let Some(state) = frontier.pop() {
            if !seen.insert(state) {
                continue;
            }
            let steps = [
                StorageSyncStep::SendCopy,
                StorageSyncStep::DeliverCopy,
                StorageSyncStep::LocalWrite(StorageValue::V0),
                StorageSyncStep::LocalWrite(StorageValue::V1),
                StorageSyncStep::LocalWrite(StorageValue::V2),
                StorageSyncStep::DeliverAckDelete,
            ];
            for step in steps {
                if let Some(next) = state.step(step) {
                    if let Some(removed) = state.removed_local_value(next) {
                        assert_eq!(
                            next.successor,
                            Some(removed),
                            "S2' violated by {step:?}: {state:?} -> {next:?}"
                        );
                    }
                    if !seen.contains(&next) {
                        frontier.push(next);
                    }
                }
            }
        }
    }
}
