#![deny(missing_docs)]
//! Delivery toward a node DID (#873).
//!
//! Delivering a payload to the node `T` and finding the owner of the position `k` are
//! different questions. [`find_successor`](crate::dht::topology::find_successor) answers the
//! second, and crossing `k` is correct there: the owner is the first node at or after `k`.
//! `T` is a node, not a position, so any node past `T` is farther from it, and a hop that
//! crosses `T` breaks the progress measure that makes Chord terminate. This module answers
//! the first question and crosses its aim at most once, by one marked handoff.
//!
//! The carrier's [`RouteStage`](crate::dht::delivery::RouteStage) is `(aim, handed_off)` with
//! `aim ∈ {T, via b}`:
//!
//! ```text
//! route(a) = Greedy*(a) · ( Deliver(a) | Handoff · Greedy*(a) · (Deliver(a) | ⊥) | ⊥ )
//! route(T) = route(T)                                   when no reply_via is named
//! route(T) = route(b) · (Deliver(T) | ⊥)                for an answer with reply_via = b
//! ```
//!
//! - `Greedy(a)`: forward to the linked known peer on `(n, a]` nearest `a`
//!   ([`route_toward`](crate::dht::delivery::route_toward));
//! - `Handoff`: no linked known peer lies on `(n, a]`; the payload goes once to the first
//!   linked known node after `n`, which lies past `a`, and the stage is marked. The node that
//!   handed off saw too little of the ring, not a wrong ring: a leaf linked only to its guard,
//!   or a joiner linked only to its bootstrap, hands off on its first hop, and the receiver's
//!   fuller view routes on greedily. A second crossing is refused, which ends the route with a
//!   typed error;
//! - `via b`: a successor or connection answer whose request named `reply_via = b` (see
//!   [`Transaction`](crate::message::Transaction)) is first delivered to `b`, which hands it
//!   to `T` over its link.
//!
//! Law (safety). Every greedy hop satisfies `next ∈ (n, aim]`, so `dist(·, aim)` strictly
//! decreases in `ℕ` and a greedy run visits each node at most once; the handoff flag only moves
//! `⊥ → ⊤`, and a second crossing is refused, so a route toward one aim is at most two greedy
//! runs joined by one handoff. With the final hop from `b` to `T`:
//!
//! ```text
//! hops(route) ≤ 2(|V| − 1) + 1 + 1 = 2|V|        (2|V| − 1 without reply_via)
//! ```
//!
//! What cycled before (#865) was the unbounded repetition of crossings; one crossing is what a
//! sparse view needs. No route cycles in any views. The hop budget ends a delivery only when a
//! correct route is longer than it: with a sparse finger table greedy delivery degenerates to a
//! walk along the successor lists, so `RelayHopBudgetExhausted` on the delivery path requires
//! `2|V| > MAX_RELAY_HOPS` (a ring whose finger table does not span the identifier space).
//! On the Chord fixpoint every greedy hop at least halves the remaining distance, so a route
//! takes at most `⌈log₂ dist(n₀, T)⌉ + 1` hops, `O(log |V|)` with high probability for random
//! identifiers.
//!
//! Law (liveness). The payload is delivered if a node on either greedy run is linked to the
//! aim. On the Chord fixpoint `pred(T)` knows `T`, so the first run delivers. A sender whose
//! view is sparse reaches every node its handoff receiver's view reaches. Whatever the 0.31
//! owner-lookup router delivered without cycling and with at most one crossing, delivery
//! delivers too. Before convergence a payload for a node no view knows fails fast, within
//! `2|V|` hops; a successor or connection answer to a node without a predecessor reaches it
//! through the peer it named.

use num_bigint::BigUint;

use super::topology::dist;
use super::topology::TopologyState;
use super::Did;

/// The stage of a route, carried by the relay carrier outside every signature.
///
/// `RouteStage = Aim × 𝔹` with `Aim = {destination} + {via b}`. A fresh carrier starts at
/// `(destination, ⊥)`, or at `(via b, ⊥)` for a successor or connection answer whose request
/// named `reply_via = b`; a handoff moves `⊥ → ⊤` and nothing moves it back, which bounds each
/// aim to two greedy runs and one crossing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RouteStage {
    /// The peer a report is first delivered to; `None` while the route aims at the
    /// destination itself.
    pub via: Option<Did>,
    /// Whether the payload has been handed past its aim once, to the first linked known node
    /// after the hop that knew no peer on the way to it; a second crossing is refused.
    pub handed_off: bool,
}

impl RouteStage {
    /// The stage every fresh carrier toward its destination starts in.
    pub const TOWARD: Self = Self {
        via: None,
        handed_off: false,
    };

    /// The start stage of a report whose request named `reply_via`: through that peer when
    /// named, otherwise straight toward the destination.
    pub const fn replying_via(reply_via: Option<Did>) -> Self {
        Self {
            via: reply_via,
            handed_off: false,
        }
    }

    /// The same aim, marked as handed off.
    pub const fn handed_off(self) -> Self {
        Self {
            via: self.via,
            handed_off: true,
        }
    }

    /// The node this stage routes toward for a payload addressed to `destination`.
    pub fn aim(self, destination: Did) -> Did {
        self.via.unwrap_or(destination)
    }
}

/// One routing decision: the peer to send to and the stage the carrier leaves in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NextHop {
    /// The linked peer that receives the payload next.
    pub peer: Did,
    /// The carrier stage after this hop.
    pub stage: RouteStage,
}

impl NextHop {
    /// A hop to `peer` leaving the carrier in `stage`.
    pub const fn new(peer: Did, stage: RouteStage) -> Self {
        Self { peer, stage }
    }

    /// A hop to `peer` in the initial stage [`RouteStage::TOWARD`]: the first hop of a fresh
    /// route whose next hop the caller fixed.
    pub const fn toward(peer: Did) -> Self {
        Self::new(peer, RouteStage::TOWARD)
    }
}

/// Pure result of one greedy delivery step toward a node (see [`route_toward`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteStep {
    /// Send to this linked known peer on `(n, a]`; the peer is `a` itself when known.
    Forward(Did),
    /// No linked known peer lies on `(n, a]`: hand the payload once to the first linked known
    /// node after `n`, which lies past `a`.
    Handoff(Did),
    /// The node has no linked known peer to route through.
    Isolated,
}

/// `Reaches(n, p, a)`: `p` lies on the half-open arc `(n, a]`, so forwarding to `p` either
/// delivers to `a` (`p = a`) or makes strict clockwise progress toward it.
fn reaches(local: Did, peer: Did, target: &BigUint) -> bool {
    peer != local && dist(local, peer) <= *target
}

/// `Known(n) ∩ Linked(n)`, where `Known(n) = succ[n] ∪ {finger[n][i]}` minus `n` itself: the
/// peers of the view this node can hand a message to now. The successor list may name peers a
/// stabilization report introduced before any link to them exists, so knowing a peer is not
/// enough.
fn linked_known_peers<'state>(
    state: &'state TopologyState,
    linked: &'state impl Fn(Did) -> bool,
) -> impl Iterator<Item = Did> + 'state {
    state
        .successors
        .iter()
        .copied()
        .chain(state.fingers.iter().flatten().copied())
        .filter(move |peer| *peer != state.local && linked(*peer))
}

/// Pure greedy step of delivery toward the node `aim` over one view and this node's link
/// predicate `linked`.
///
/// ```text
/// ∃ p ∈ Known ∩ Linked. p ∈ (n, a] ──▶ Forward(argmax dist(n, p))    (p = a delivers)
///            │ no
///            ▼
/// ∃ p ∈ Known ∩ Linked            ──▶ Handoff(argmin dist(n, p))    (past a: the first
///            │ no                                                    linked node after it)
///            ▼
///         Isolated
/// ```
///
/// Law (progress). `Forward(p)` satisfies `dist(p, a) < dist(n, a)`, so a chain of forwards
/// strictly decreases a measure in `ℕ` and visits each node at most once. `Handoff(h)` is the
/// only step that passes `a`; since no linked known peer lies on `(n, a]`, `h` is the first
/// linked known node after `a`.
///
/// Unlike [`find_successor`](crate::dht::topology::find_successor), which answers
/// `Local(head)` for `a ∈ (n, head]` and so lets a hop that does not know `a` pass it and then
/// route greedily again from the far side, circling among the nodes whose views skip `a`
/// (#873), the pass here is explicit and happens at most once per aim.
pub fn route_toward(state: &TopologyState, aim: Did, linked: impl Fn(Did) -> bool) -> RouteStep {
    let target = dist(state.local, aim);
    let peers = || linked_known_peers(state, &linked);
    match peers()
        .filter(|peer| reaches(state.local, *peer, &target))
        .max_by_key(|peer| dist(state.local, *peer))
    {
        Some(next) => RouteStep::Forward(next),
        None => peers()
            .min_by_key(|peer| dist(state.local, *peer))
            .map_or(RouteStep::Isolated, RouteStep::Handoff),
    }
}

/// `ReplyVia(n)`: the peer `n` names for its successor and connection answers while no node is
/// known to route to it, i.e. while it has no predecessor; `None` once a predecessor has
/// notified it.
///
/// A predecessor `p` notifies `n` iff `p`'s successor is `n`, i.e. iff `p` knows `n`; from then
/// on greedy delivery reaches `n` through `p`. Before that (while joining, and again after the
/// predecessor departs until a new one notifies), only `n`'s own links know it, so its answers
/// must return through one of them: its nearest successor it is linked to, which for a joiner
/// is its bootstrap. A successor entry without a link cannot hand an answer on, so it is
/// skipped.
pub fn reply_via(state: &TopologyState, linked: impl Fn(Did) -> bool) -> Option<Did> {
    if state.predecessor.is_some() {
        return None;
    }
    state
        .successors
        .iter()
        .copied()
        .find(|successor| *successor != state.local && linked(*successor))
}

/// One delivery step at `view.local` for a payload addressed to `destination` whose carrier
/// is in `stage`; `None` ends the route undelivered.
///
/// `linked` is the transport's direct-link predicate, a fact of this node and not of the view.
///
/// ```text
/// linked(T)                 ──▶ Deliver(T)                      (any stage)
/// stage = (via n, _)        ──▶ ⊥                               (n lost its link to T)
/// linked(a)                 ──▶ Deliver(a)
/// otherwise                 ──▶ route_toward(view, a, linked) ∈
///                                 { Forward(p)  ↦ (p, stage)
///                                 , Handoff(h)  ↦ (h, (a, ⊤))   if stage = (a, ⊥)
///                                 , Handoff(_)  ↦ ⊥             if stage = (a, ⊤)   (no second crossing)
///                                 , Isolated    ↦ ⊥ }
/// ```
pub fn delivery_step(
    view: &TopologyState,
    destination: Did,
    stage: RouteStage,
    linked: impl Fn(Did) -> bool,
) -> Option<NextHop> {
    if linked(destination) {
        return Some(NextHop::new(destination, stage));
    }
    let aim = stage.aim(destination);
    if aim != destination {
        if aim == view.local {
            return None;
        }
        if linked(aim) {
            return Some(NextHop::new(aim, stage));
        }
    }
    match route_toward(view, aim, linked) {
        RouteStep::Forward(peer) => Some(NextHop::new(peer, stage)),
        RouteStep::Handoff(peer) if !stage.handed_off => {
            Some(NextHop::new(peer, stage.handed_off()))
        }
        RouteStep::Handoff(_) | RouteStep::Isolated => None,
    }
}

/// The decisions a node takes when it originates a request toward `destination`, from one
/// view: the first hop, and the `reply_via` its transaction names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origination {
    /// The first hop; `None` when no route leaves this node.
    pub hop: Option<NextHop>,
    /// See [`reply_via`].
    pub reply_via: Option<Did>,
}

/// `Origination(view, T) = (delivery_step(view, T, TOWARD), ReplyVia(view))`, both read from
/// the same view so the hint and the hop describe one topology.
pub fn origination(
    view: &TopologyState,
    destination: Did,
    linked: impl Fn(Did) -> bool,
) -> Origination {
    Origination {
        hop: delivery_step(view, destination, RouteStage::TOWARD, &linked),
        reply_via: reply_via(view, &linked),
    }
}

/// Checked model of delivery: safety laws, and the converged and unconverged regimes.
#[cfg(test)]
mod test_model;
