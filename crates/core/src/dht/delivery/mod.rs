#![deny(missing_docs)]
//! Delivery toward a node DID (#873).
//!
//! Delivering a payload to the node `T` and finding the owner of the position `k` are
//! different questions. [`find_successor`](crate::dht::topology::find_successor) answers the
//! second, and crossing `k` is correct there: the owner is the first node at or after `k`.
//! `T` is a node, not a position, so any node past `T` is farther from it, and a hop that
//! crosses `T` breaks the progress measure that makes Chord terminate. This module answers
//! the first question and never crosses its aim except by one marked, terminal handoff.
//!
//! The carrier's [`RouteStage`](crate::dht::delivery::RouteStage) is `(aim, handed_off)` with
//! `aim ∈ {T, via b}`:
//!
//! ```text
//! route(T) = Greedy*(T) · ( Deliver | Handoff · (Deliver | ⊥) )                  reply_via = ∅
//! route(T) = Greedy*(b) · ( Deliver(b) | Handoff · (Deliver(b) | ⊥) ) · (Deliver(T) | ⊥)   reply_via = b
//! ```
//!
//! - `Greedy(a)`: forward to the linked known peer on `(n, a]` nearest `a`
//!   ([`route_toward`](crate::dht::topology::route_toward));
//! - `Handoff`: no linked known peer lies on `(n, a]`; the payload goes once to the first
//!   linked known node after `n`, which lies past `a`, and the stage is marked; the receiver
//!   delivers over a direct link or ends the route with a typed error, and never routes
//!   greedily again;
//! - `via b`: a report whose request named `reply_via = b` (see
//!   [`Transaction`](crate::message::Transaction)) is first delivered to `b`, which hands it
//!   to `T` over its link.
//!
//! Law (safety). Every greedy hop satisfies `next ∈ (n, aim]`, so `dist(·, aim)` strictly
//! decreases in `ℕ` and a greedy run visits each node at most once; the handoff flag only moves
//! `⊥ → ⊤`, and reaching `b` in stage `(via b, _)` never starts a second greedy run. A route is
//! therefore one greedy run (at most `|V| − 1` hops), at most one handoff, and at most two
//! terminal deliveries (to `b`, then from `b` to `T`):
//!
//! ```text
//! hops(route) ≤ (|V| − 1) + 1 + 2 = |V| + 2        (|V| + 1 without reply_via)
//! ```
//!
//! No route cycles in any views. The hop budget ends a delivery only when a correct route is
//! longer than it: with a sparse finger table greedy delivery degenerates to a walk along the
//! successor lists, so `RelayHopBudgetExhausted` on the delivery path requires
//! `|V| + 2 > MAX_RELAY_HOPS` (a ring whose finger table does not span the identifier space);
//! with spanning fingers a greedy run takes `O(log |V|)` hops.
//!
//! Law (liveness). The payload is delivered if a node on the greedy prefix is linked to the
//! aim, or the handoff receiver is. On the Chord fixpoint `pred(T)` knows `T`. Before
//! convergence a payload for a node no view knows fails fast; a report to a node without a
//! predecessor reaches it through the peer it named.

use super::topology::reply_via;
use super::topology::route_toward;
use super::topology::RouteStep;
use super::topology::TopologyState;
use super::Did;

/// The stage of a route, carried by the relay carrier outside every signature.
///
/// `RouteStage = Aim × 𝔹` with `Aim = {destination} + {via b}`. A fresh carrier starts at
/// `(destination, ⊥)`, or at `(via b, ⊥)` for a report whose request named `reply_via = b`;
/// a handoff moves `⊥ → ⊤` and nothing moves it back, which bounds each aim to one greedy run
/// and one crossing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RouteStage {
    /// The peer a report is first delivered to; `None` while the route aims at the
    /// destination itself.
    pub via: Option<Did>,
    /// Whether the payload has been handed past its aim to the owner of the aim's successor
    /// position.
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

/// One delivery step at `view.local` for a payload addressed to `destination` whose carrier
/// is in `stage`; `None` ends the route undelivered.
///
/// `linked` is the transport's direct-link predicate, a fact of this node and not of the view.
///
/// ```text
/// linked(T)                 ──▶ Deliver(T)                      (any stage)
/// stage = (via n, _)        ──▶ ⊥                               (n lost its link to T)
/// stage = (a, ⊤)            ──▶ linked(a) ? Deliver(a) : ⊥      (handoff receiver)
/// stage = (a, ⊥)            ──▶ linked(a) ? Deliver(a) : route_toward(view, a, linked) ∈
///                                 { Forward(p) ↦ (p, (a, ⊥)), Handoff(h) ↦ (h, (a, ⊤)), Isolated ↦ ⊥ }
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
    if stage.handed_off {
        return None;
    }
    match route_toward(view, aim, linked) {
        RouteStep::Forward(peer) => Some(NextHop::new(peer, stage)),
        RouteStep::Handoff(peer) => Some(NextHop::new(peer, stage.handed_off())),
        RouteStep::Isolated => None,
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
        hop: delivery_step(view, destination, RouteStage::TOWARD, linked),
        reply_via: reply_via(view),
    }
}

/// Checked model of delivery: safety laws, and the converged and unconverged regimes.
#[cfg(test)]
mod test_model;
