#![deny(missing_docs)]
//! Delivery toward a node DID (#873).
//!
//! Delivering a payload to the node `T` and finding the owner of the position `k` are
//! different questions. [`find_successor`](super::topology::find_successor) answers the
//! second, and crossing `k` is correct there: the owner is the first node at or after `k`.
//! `T` is a node, not a position, so any node past `T` is farther from it, and a hop that
//! crosses `T` breaks the progress measure that makes Chord terminate. This module answers
//! the first question and never crosses its aim except by one marked, terminal handoff.
//!
//! The carrier's [`RouteStage`] is `(aim, handed_off)` with `aim ∈ {T, via b}`:
//!
//! ```text
//! route(T) = Greedy*(T) · ( Deliver | Handoff · (Deliver | ⊥) )                  reply_via = ∅
//! route(T) = Greedy*(b) · ( Deliver(b) | Handoff · (Deliver(b) | ⊥) ) · (Deliver(T) | ⊥)   reply_via = b
//! ```
//!
//! - `Greedy(a)`: forward to the known peer on `(n, a]` nearest `a` ([`route_toward`]);
//! - `Handoff`: no known peer lies on `(n, a]`; the payload goes once to `head(n)`, the owner
//!   of `a⁺` in `n`'s view, and the stage is marked; the receiver delivers over a direct link
//!   or ends the route with a typed error, and never routes greedily again;
//! - `via b`: a report whose request named `reply_via = b` (see
//!   [`Transaction`](crate::message::Transaction)) is first delivered to `b`, which hands it
//!   to `T` over its link.
//!
//! Law (safety). Every greedy hop satisfies `next ∈ (n, aim]`, so `dist(·, aim)` strictly
//! decreases in `ℕ` and no state repeats; the stage only moves up the chain
//! `(T,⊥) < (T,⊤)`, resp. `(b,⊥) < (b,⊤)`. So a route crosses each aim at most once (`T`
//! itself when no `reply_via` is named) and ends within `2(|V| + 1) + 1` hops in any views: the
//! hop budget is never what ends it.
//!
//! Law (liveness). The payload is delivered if a node on the greedy prefix is linked to the
//! aim, or the handoff receiver is. On the Chord fixpoint `pred(T)` knows `T`. Before
//! convergence a payload for a node no view knows fails fast; a report to a joiner reaches it
//! through the bootstrap it named.

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
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize,
)]
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
/// stage = (a, ⊥)            ──▶ linked(a) ? Deliver(a) : route_toward(view, a) ∈
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
    if aim == view.local {
        return None;
    }
    if linked(aim) {
        return Some(NextHop::new(aim, stage));
    }
    if stage.handed_off {
        return None;
    }
    match route_toward(view, aim) {
        RouteStep::Forward(peer) => Some(NextHop::new(peer, stage)),
        RouteStep::Handoff(peer) => Some(NextHop::new(peer, stage.handed_off())),
        RouteStep::Isolated => None,
    }
}

#[cfg(test)]
mod tests;
